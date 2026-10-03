//! Dossiers de vidéos : cartes SD détectées, navigation dans les dossiers du PC et surveillance
//! des nouveaux fichiers (scan automatique).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bike360_core::insta360;
use serde_json::{json, Value};

use crate::app::App;

/// Pause entre deux relevés des dossiers surveillés.
const WATCH_EVERY: Duration = Duration::from_secs(8);
/// Profondeur maximale des sous-dossiers parcourus pour la surveillance.
const WATCH_DEPTH: usize = 4;
/// Sous-dossiers dont on compte les vidéos dans le navigateur de dossiers.
const BROWSE_MAX_DIRS: usize = 400;

/// Nom d'un fichier vidéo de la caméra (.insv / .lrv).
fn is_video(name: &str) -> bool {
    let l = name.to_lowercase();
    (l.starts_with("vid_") || l.starts_with("lrv_")) && (l.ends_with(".insv") || l.ends_with(".lrv"))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

// ------------------------------------------------------------------ cartes SD

/// Points de montage de volumes amovibles (/run/media, /media, /mnt).
fn removable_mounts() -> Vec<PathBuf> {
    let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut out: Vec<PathBuf> = vec![];
    for line in text.lines() {
        let Some(mp) = line.split_whitespace().nth(1) else { continue };
        let mp = mp.replace("\\040", " ");   // les espaces sont codés ainsi dans /proc
        if ["/run/media/", "/media/", "/mnt/"].iter().any(|p| mp.starts_with(p)) && !out.iter().any(|o| o == Path::new(&mp)) {
            out.push(PathBuf::from(mp));
        }
    }
    out
}

/// Dossier DCIM d'un volume (le nom peut varier de casse).
fn dcim_of(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root).ok()?.flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("DCIM") && e.path().is_dir())
        .map(|e| e.path())
}

/// Cartes détectées : [{path, label, sessions, added}] (volumes montés contenant un DCIM avec des vidéos de la caméra).
pub fn detect(app: &App) -> Vec<Value> {
    let known = app.source_folders();
    removable_mounts().into_iter()
        .filter_map(|mp| {
            let dcim = dcim_of(&mp)?;
            let n = insta360::scan(&dcim).len();
            if n == 0 {
                return None;
            }
            let path = std::fs::canonicalize(&dcim).unwrap_or(dcim).display().to_string();
            Some(json!({"path": path, "label": mp.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                        "sessions": n, "added": known.contains(&path)}))
        })
        .collect()
}

// ------------------------------------------------------------------ navigateur de dossiers

/// Contenu d'un dossier pour le navigateur : sous-dossiers (nombre de vidéos de la caméra à l'intérieur),
/// dossier parent et raccourcis. Les fichiers et dossiers cachés ne sont pas listés.
pub fn browse(path: &str) -> Result<Value> {
    let raw = if path.is_empty() { home() } else if let Some(rest) = path.strip_prefix('~') { PathBuf::from(format!("{}{rest}", home().display())) } else { PathBuf::from(path) };
    let dir = std::fs::canonicalize(&raw).with_context(|| format!("dossier introuvable : {}", raw.display()))?;
    if !dir.is_dir() {
        bail!("pas un dossier : {}", dir.display());
    }
    let entries = std::fs::read_dir(&dir).with_context(|| format!("lecture impossible : {}", dir.display()))?;
    let mut here = 0usize;
    let mut subs: Vec<(String, PathBuf)> = vec![];
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            subs.push((name, p));
        } else if is_video(&name) {
            here += 1;
        }
    }
    subs.sort_by_key(|(n, _)| n.to_lowercase());
    let dirs: Vec<Value> = subs.iter().enumerate().map(|(i, (name, p))| {
        // vidéos directement dans le sous-dossier (au-delà d'une limite, on ne compte plus)
        let videos = if i < BROWSE_MAX_DIRS {
            std::fs::read_dir(p).map(|d| d.flatten().filter(|f| is_video(&f.file_name().to_string_lossy())).count()).ok()
        } else {
            None
        };
        json!({"name": name, "path": p.display().to_string(), "videos": videos})
    }).collect();
    let mut shortcuts = vec![json!({"label": "Maison", "path": home().display().to_string()})];
    for m in removable_mounts() {
        let label = m.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| m.display().to_string());
        shortcuts.push(json!({"label": format!("💾 {label}"), "path": m.display().to_string()}));
    }
    Ok(json!({"path": dir.display().to_string(), "parent": dir.parent().map(|p| p.display().to_string()),
              "videos_here": here, "dirs": dirs, "shortcuts": shortcuts}))
}

// ------------------------------------------------------------------ scan automatique

/// Fichiers vidéo (chemin + taille) des dossiers surveillés présents.
fn snapshot(app: &App) -> BTreeSet<(String, u64)> {
    fn walk(dir: &Path, depth: usize, out: &mut BTreeSet<(String, u64)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if depth > 0 && !name.starts_with('.') {
                    walk(&p, depth - 1, out);
                }
            } else if is_video(&name) {
                out.insert((p.display().to_string(), e.metadata().map_or(0, |m| m.len())));
            }
        }
    }
    let mut out = BTreeSet::new();
    for f in app.source_folders() {
        walk(Path::new(&f), WATCH_DEPTH, &mut out);
    }
    out
}

/// Surveille les dossiers : quand de nouveaux fichiers apparaissent (carte branchée, copie terminée),
/// relance l'analyse. Un fichier encore en cours de copie (taille qui change) attend la fin.
/// Un retrait de fichiers ne déclenche rien : les sessions déjà connues restent utilisables.
pub fn watch(app: Arc<App>) {
    let mut known = snapshot(&app);
    let mut pending: Option<BTreeSet<(String, u64)>> = None;
    loop {
        std::thread::sleep(WATCH_EVERY);
        if app.scan.lock().unwrap().get("state").and_then(Value::as_str) == Some("running") {
            continue;
        }
        let now = snapshot(&app);
        let fresh = now.difference(&known).count();
        if fresh == 0 {
            known = now;   // retraits ou rien de neuf
            pending = None;
            continue;
        }
        if pending.as_ref() == Some(&now) {   // stable depuis deux relevés : la copie est finie
            known = now;
            pending = None;
            eprintln!("Nouveaux fichiers détectés ({fresh}) : analyse");
            app.rescan();
        } else {
            pending = Some(now);
        }
    }
}
