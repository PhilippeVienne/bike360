//! Client minimal de l'API GeoRide (https://api.georide.com).
//!
//! Identifiants lus depuis ~/.config/bike360/georide.env (chmod 600) :
//!     GEORIDE_EMAIL=...
//!     GEORIDE_PASSWORD=...
//! Le jeton obtenu est mis en cache dans ~/.config/bike360/georide.token.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const API: &str = "https://api.georide.com";

fn conf_dir() -> PathBuf {
    let config = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config");
    let dir = config.join("bike360");
    // nom d'avant Bike360 : identifiants laissés en place tant qu'ils ne sont pas dans le nouveau dossier
    let old = config.join("insta-build");
    if !dir.join("georide.env").exists() && old.join("georide.env").exists() { old } else { dir }
}

fn load_env() -> Result<HashMap<String, String>> {
    let path = conf_dir().join("georide.env");
    let meta = std::fs::metadata(&path)
        .with_context(|| format!("crée {path:?} avec GEORIDE_EMAIL=... et GEORIDE_PASSWORD=... (chmod 600)"))?;
    if meta.permissions().mode() & 0o077 != 0 {
        bail!("{path:?} est lisible par d'autres utilisateurs : chmod 600 {path:?}");
    }
    let mut env = HashMap::new();
    for line in std::fs::read_to_string(&path)?.lines() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            env.insert(k.trim().to_string(), v.trim().trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    Ok(env)
}

fn login() -> Result<String> {
    let env = load_env()?;
    let body = serde_json::json!({"email": env.get("GEORIDE_EMAIL"), "password": env.get("GEORIDE_PASSWORD")});
    let account: Value = ureq::post(&format!("{API}/user/login")).send_json(body)?.into_json()?;
    let token = account["authToken"].as_str().context("réponse de connexion sans jeton")?.to_string();
    let dir = conf_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("georide.token");
    std::fs::write(&path, &token)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(token)
}

fn call(path: &str, params: &[(&str, &str)]) -> Result<Value> {
    let token_file = conf_dir().join("georide.token");
    let token = match std::fs::read_to_string(&token_file) {
        Ok(t) => t.trim().to_string(),
        Err(_) => login()?,
    };
    let get = |token: &str| {
        let mut req = ureq::get(&format!("{API}{path}")).set("Authorization", &format!("Bearer {token}"));
        for (k, v) in params {
            req = req.query(k, v);
        }
        req.call()
    };
    match get(&token) {
        Ok(r) => Ok(r.into_json()?),
        Err(ureq::Error::Status(401 | 403, _)) => Ok(get(&login()?)?.into_json()?),
        Err(e) => Err(e.into()),
    }
}

/// Trackers du compte (id, nom, modèle) — `georide-cli trackers`, ou pour choisir un
/// `tracker` à passer explicitement quand le compte en a plusieurs.
pub fn list_trackers() -> Result<Value> {
    call("/user/trackers", &[])
}

fn tracker_id(tracker: Option<&str>) -> Result<String> {
    if let Some(id) = tracker {
        return Ok(id.to_string());
    }
    let trackers = list_trackers()?;
    let list = trackers.as_array().context("liste de trackers attendue")?;
    if list.len() != 1 {
        bail!("plusieurs trackers GeoRide : préciser lequel (voir list_trackers)");
    }
    Ok(list[0]["trackerId"].to_string().trim_matches('"').to_string())
}

/// Positions GeoRide entre deux dates ISO (fixtime UTC ; attention, `speed` est en nœuds).
pub fn fetch_positions(start: &str, end: &str) -> Result<Value> {
    fetch_positions_for(start, end, None)
}

/// Comme `fetch_positions`, avec un tracker explicite (sinon celui par défaut si un seul).
pub fn fetch_positions_for(start: &str, end: &str, tracker: Option<&str>) -> Result<Value> {
    let id = tracker_id(tracker)?;
    call(&format!("/tracker/{id}/trips/positions"), &[("from", start), ("to", end)])
}

/// Trajets découpés par GeoRide entre deux dates ISO (début, fin, distance, durée ; pas le
/// tracé détaillé — voir `fetch_positions` avec les bornes du trajet pour ça).
pub fn fetch_trips(start: &str, end: &str) -> Result<Value> {
    fetch_trips_for(start, end, None)
}

/// Comme `fetch_trips`, avec un tracker explicite (sinon celui par défaut si un seul).
pub fn fetch_trips_for(start: &str, end: &str, tracker: Option<&str>) -> Result<Value> {
    let id = tracker_id(tracker)?;
    call(&format!("/tracker/{id}/trips"), &[("from", start), ("to", end)])
}
