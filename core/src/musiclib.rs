//! Bibliothèque de musiques libres (CC BY 4.0) : catalogue Incompetech de Kevin MacLeod.
//!
//! Le catalogue (titre, durée, tempo, ambiance, instruments) est mis en cache une semaine.
//! Une musique choisie est téléchargée dans data/music/ avec son crédit, affiché à la fin du
//! montage comme l'exige la licence (titre, auteur, source, licence).

use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::paths;

const CATALOG_URL: &str = "https://incompetech.com/music/royalty-free/pieces.json";
const MP3_URL: &str = "https://incompetech.com/music/royalty-free/mp3-royaltyfree/";
const USER_AGENT: &str = "insta-build/1.0 (outil personnel de montage)";
const CATALOG_MAX_AGE: Duration = Duration::from_secs(7 * 86400);
const ARTIST: &str = "Kevin MacLeod";
const LICENSE: &str = "CC BY 4.0";
const LICENSE_URL: &str = "creativecommons.org/licenses/by/4.0/";
/// Ambiances utiles pour des vidéos de moto (libellés d'Incompetech → français).
pub const MOODS: [(&str, &str); 10] = [
    ("Driving", "Entraînant"), ("Uplifting", "Enthousiaste"), ("Epic", "Épique"), ("Action", "Action"),
    ("Bright", "Lumineux"), ("Grooving", "Groove"), ("Relaxed", "Détendu"), ("Calming", "Calme"),
    ("Intense", "Intense"), ("Dark", "Sombre"),
];

fn catalog_path() -> PathBuf {
    paths::cache().join("incompetech.json")
}

pub fn music_dir() -> PathBuf {
    paths::data().join("music")
}

fn credits_path() -> PathBuf {
    music_dir().join("credits.json")
}

fn get(url: &str, timeout: u64) -> Result<Vec<u8>> {
    let resp = ureq::get(url).set("User-Agent", USER_AGENT).timeout(Duration::from_secs(timeout)).call()?;
    let mut buf = vec![];
    resp.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

/// Encodage d'URL d'un nom de fichier (comme urllib.parse.quote : « / » conservé).
fn quote(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Catalogue complet (liste de pièces), rafraîchi au plus une fois par semaine.
pub fn catalog() -> Result<Vec<Map<String, Value>>> {
    let path = catalog_path();
    let stale = std::fs::metadata(&path).and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > CATALOG_MAX_AGE)
        .unwrap_or(true);
    if stale {
        let fresh = get(CATALOG_URL, 30).and_then(|d| {
            serde_json::from_slice::<Value>(&d)?;
            Ok(d)
        });
        match fresh {
            Ok(d) => {
                std::fs::create_dir_all(paths::cache())?;
                std::fs::write(&path, d)?;
            }
            Err(e) if !path.exists() => return Err(e),
            Err(_) => {}
        }
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(&path)?)?)
}

fn seconds(length: Option<&Value>) -> u64 {
    let Some(s) = length.and_then(Value::as_str) else { return 0 };
    let parts: Vec<Option<u64>> = s.split(':').map(|x| x.trim().parse().ok()).collect();
    match parts.as_slice() {
        [Some(h), Some(m), Some(s)] => h * 3600 + m * 60 + s,
        _ => 0,
    }
}

fn text(p: &Map<String, Value>, k: &str) -> String {
    match p.get(k) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

#[derive(Debug, Serialize)]
pub struct Piece {
    pub title: String,
    pub filename: String,
    pub seconds: u64,
    pub bpm: Value,
    pub uploaded: String,
    pub feel: String,
    pub description: String,
    pub instruments: String,
    pub preview: String,
}

/// Pièces correspondant à une ambiance et/ou à des mots (titre, description, instruments).
pub fn search(query: &str, mood: &str, min_s: u64, limit: usize) -> Result<Vec<Piece>> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    let mut out = vec![];
    for p in catalog()? {
        let feel = text(&p, "feel");
        let feels: Vec<&str> = feel.split(',').map(str::trim).collect();
        if !mood.is_empty() && !feels.contains(&mood) {
            continue;
        }
        let all = ["title", "description", "instruments", "feel"].map(|k| text(&p, k)).join(" ").to_lowercase();
        if words.iter().any(|w| !all.contains(w.as_str())) {
            continue;
        }
        let secs = seconds(p.get("length"));
        if secs < min_s {
            continue;
        }
        let filename = text(&p, "filename");
        out.push(Piece {
            title: text(&p, "title"),
            preview: format!("{MP3_URL}{}", quote(&filename)),
            filename,
            seconds: secs,
            bpm: p.get("bpm").cloned().unwrap_or(Value::Null),
            uploaded: text(&p, "uploaded"),
            feel: feels.iter().filter(|f| !f.is_empty())
                .map(|f| MOODS.iter().find(|(k, _)| k == f).map_or(*f, |(_, v)| *v))
                .collect::<Vec<_>>().join(", "),
            description: text(&p, "description"),
            instruments: text(&p, "instruments"),
        });
    }
    out.sort_by(|a, b| b.uploaded.cmp(&a.uploaded)); // les plus récentes d'abord (tri stable)
    out.truncate(limit);
    Ok(out)
}

pub fn credits() -> Map<String, Value> {
    std::fs::read_to_string(credits_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Télécharge une pièce du catalogue dans data/music/ et enregistre son crédit.
pub fn download(filename: &str) -> Result<String> {
    let cat = catalog()?;
    let Some(piece) = cat.iter().find(|p| text(p, "filename") == filename) else { bail!("pièce inconnue") };
    std::fs::create_dir_all(music_dir())?;
    let target = music_dir().join(filename);
    if !target.exists() {
        let data = get(&format!("{MP3_URL}{}", quote(filename)), 120).context("téléchargement")?;
        std::fs::write(&target, data)?;
    }
    let mut c = credits();
    c.insert(filename.into(), json!({"title": text(piece, "title"), "artist": ARTIST, "source": "incompetech.com",
                                     "license": LICENSE, "license_url": LICENSE_URL}));
    std::fs::write(credits_path(), serde_json::to_string_pretty(&c)?)?;
    Ok(filename.into())
}

/// Lignes de crédit d'une musique de la bibliothèque, ou [] (musique envoyée par l'utilisateur).
pub fn credit_lines(filename: &str) -> Vec<String> {
    let c = credits();
    let Some(c) = c.get(filename) else { return vec![] };
    let f = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    vec![format!("Musique : « {} » — {} ({})", f("title"), f("artist"), f("source")),
         format!("Licence {} · {}", f("license"), f("license_url"))]
}
