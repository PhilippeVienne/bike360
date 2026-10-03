//! Fichiers audio du montage (data/music) : durée et forme d'onde pour la frise de l'éditeur,
//! pistes prêtes pour ffmpeg à l'export.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use bike360_core::finishing;
use serde_json::Value;

use crate::app::{music_dir, App};

/// Valeurs de la forme d'onde par seconde de son.
pub const PEAKS_PER_S: usize = 20;
const PEAKS_RATE: usize = 4000;
const PEAKS_MAX_S: f64 = 3600.0;

type Key = (String, SystemTime);

fn cache<T>(cell: &'static OnceLock<Mutex<HashMap<Key, T>>>) -> &'static Mutex<HashMap<Key, T>> {
    cell.get_or_init(Default::default)
}

/// Chemin d'un fichier audio du dossier de musiques (nom simple, sans dossier).
pub fn path_of(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return None;
    }
    let p = music_dir().join(name);
    p.is_file().then_some(p)
}

fn stamp(p: &std::path::Path) -> SystemTime {
    p.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Durée d'un fichier audio (s), mémorisée tant que le fichier ne change pas.
pub fn seconds(name: &str) -> Result<f64> {
    static CACHE: OnceLock<Mutex<HashMap<Key, f64>>> = OnceLock::new();
    let p = path_of(name).context("fichier inconnu")?;
    let key = (name.to_string(), stamp(&p));
    if let Some(d) = cache(&CACHE).lock().unwrap().get(&key) {
        return Ok(*d);
    }
    let (d, has_audio) = finishing::probe(&p)?;
    if !has_audio {
        bail!("{name} : pas de son");
    }
    cache(&CACHE).lock().unwrap().insert(key, d);
    Ok(d)
}

/// Forme d'onde : amplitude maximale (0 à 1) de chaque vingtième de seconde.
pub fn peaks(name: &str) -> Result<Vec<f32>> {
    static CACHE: OnceLock<Mutex<HashMap<Key, Vec<f32>>>> = OnceLock::new();
    let p = path_of(name).context("fichier inconnu")?;
    let key = (name.to_string(), stamp(&p));
    if let Some(v) = cache(&CACHE).lock().unwrap().get(&key) {
        return Ok(v.clone());
    }
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-i"]).arg(&p)
        .args(["-vn", "-ac", "1", "-ar", &PEAKS_RATE.to_string(), "-t", &PEAKS_MAX_S.to_string(), "-f", "s16le", "-"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().context("ffmpeg")?;
    let mut raw = vec![];
    child.stdout.take().context("ffmpeg")?.read_to_end(&mut raw)?;
    child.wait()?;
    let step = PEAKS_RATE / PEAKS_PER_S;
    let samples: Vec<i16> = raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let v: Vec<f32> = samples.chunks(step).map(|c| c.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0).collect();
    cache(&CACHE).lock().unwrap().insert(key, v.clone());
    Ok(v)
}

/// Fichiers audio du dossier avec leur durée : [{name, seconds}] (un fichier illisible est omis).
pub fn list(app: &App) -> Vec<Value> {
    app.music_files().into_iter()
        .filter_map(|n| seconds(&n).ok().map(|s| serde_json::json!({"name": n, "seconds": s})))
        .collect()
}

/// Pistes du style prêtes pour l'export : fichier présent et lisible, sinon la piste est ignorée.
pub fn resolve<'a>(style: &'a Value) -> Vec<finishing::Track<'a>> {
    style["audio_tracks"].as_array().into_iter().flatten()
        .filter_map(|t| {
            let name = t["file"].as_str()?;
            Some(finishing::Track { path: path_of(name)?, seconds: seconds(name).ok()?, spec: t })
        })
        .collect()
}
