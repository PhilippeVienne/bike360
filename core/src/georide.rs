//! Client minimal de l'API GeoRide (https://api.georide.com).
//!
//! Identifiants lus depuis ~/.config/insta-build/georide.env (chmod 600) :
//!     GEORIDE_EMAIL=...
//!     GEORIDE_PASSWORD=...
//! Le jeton obtenu est mis en cache dans ~/.config/insta-build/georide.token.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde_json::Value;

const API: &str = "https://api.georide.com";

fn conf_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config").join("insta-build")
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

fn tracker_id() -> Result<String> {
    let trackers = call("/user/trackers", &[])?;
    let list = trackers.as_array().context("liste de trackers attendue")?;
    if list.len() != 1 {
        bail!("plusieurs trackers GeoRide : préciser lequel");
    }
    Ok(list[0]["trackerId"].to_string().trim_matches('"').to_string())
}

/// Positions GeoRide entre deux dates ISO (fixtime UTC ; attention, `speed` est en nœuds).
pub fn fetch_positions(start: &str, end: &str) -> Result<Value> {
    let id = tracker_id()?;
    call(&format!("/tracker/{id}/trips/positions"), &[("from", start), ("to", end)])
}
