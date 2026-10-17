//! Passage des identifiants de session de `VID_<date>_<heure>` à `VID_<date>_<heure>_<caméra>` :
//! renomme les fichiers de data/ et d'exports/ nommés d'après une session et corrige les
//! identifiants cités dans les fichiers JSON. Rien n'est écrit sans `apply`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::json;

use crate::insta360;

/// Dossiers (sous la racine) dont les entrées portent un identifiant de session dans leur nom.
const NAMED_DIRS: [&str; 7] = ["data/cache", "data/cache/privacy", "data/cache/privacy_render", "data/selections",
                               "data/selections/historique", "data/privacy", "exports"];
/// Fichiers JSON (sous la racine) qui citent des identifiants, en plus de ceux renommés.
const CITING_FILES: [&str; 3] = ["data/project.json", "data/overrides.json", "data/positions.json"];
const JOURNAL: &str = "data/migration-ids.json";

/// Ce que la migration ferait : renommages (de, vers) et fichiers JSON à corriger, chemins sous la racine.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub renames: Vec<(PathBuf, PathBuf)>,
    pub edits: Vec<PathBuf>,
}

/// `name` est-il nommé d'après la session `old` (identifiant sans caméra) ? Un nom où `old` est
/// déjà suivi d'un suffixe de caméra appartient à une session déjà migrée.
fn belongs(name: &str, old: &str) -> bool {
    let Some(rest) = name.strip_prefix(old) else { return false };
    match rest.chars().next() {
        None | Some('.') => true,
        Some('_') => {
            let token = rest[1..].split(['_', '.']).next().unwrap_or("");
            !insta360::is_camera_suffix(token)
        }
        _ => false,
    }
}

/// Remplace chaque ancien identifiant par le nouveau, sauf là où il est déjà suivi d'un suffixe de
/// caméra (session déjà migrée, ou même instant filmé par une autre caméra).
fn replace_ids(text: &str, mapping: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (old, new) in mapping {
        let mut done = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(old.as_str()) {
            let after = &rest[i + old.len()..];
            let token = after.strip_prefix('_').map(|r| r.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or(""));
            done.push_str(&rest[..i]);
            done.push_str(if token.is_some_and(insta360::is_camera_suffix) { old } else { new });
            rest = after;
        }
        done.push_str(rest);
        out = done;
    }
    out
}

/// Identifiants sans caméra encore présents dans les données (noms de fichiers et fichiers qui les citent).
pub fn old_ids(root: &Path) -> BTreeSet<String> {
    let re = Regex::new(r"VID_\d{8}_\d{6}").unwrap();
    let mut out = BTreeSet::new();
    for dir in NAMED_DIRS {
        for e in std::fs::read_dir(root.join(dir)).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(m) = re.find(&name).filter(|m| m.start() == 0 && belongs(&name, m.as_str())) {
                out.insert(m.as_str().to_string());
            }
        }
    }
    for file in CITING_FILES {
        let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
        for m in re.find_iter(&text) {
            let token = text[m.end()..].strip_prefix('_').map(|r| r.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or(""));
            if !token.is_some_and(insta360::is_camera_suffix) {
                out.insert(m.as_str().to_string());
            }
        }
    }
    out
}

/// Plan de migration pour les correspondances (ancien, nouveau) données.
pub fn plan(root: &Path, mapping: &[(String, String)]) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut citing: Vec<PathBuf> = CITING_FILES.iter().map(PathBuf::from).collect();
    for dir in NAMED_DIRS {
        let Ok(rd) = std::fs::read_dir(root.join(dir)) else { continue };
        let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        for name in names {
            let rel = Path::new(dir).join(&name);
            match mapping.iter().find(|(old, _)| belongs(&name, old)) {
                Some((old, new)) => {
                    let to = Path::new(dir).join(format!("{new}{}", &name[old.len()..]));
                    if root.join(&to).exists() {
                        bail!("{} existe déjà : migration interrompue", to.display());
                    }
                    if name.ends_with(".json") {
                        citing.push(rel.clone());
                    }
                    plan.renames.push((rel, to));
                }
                None if name.ends_with(".json") && root.join(&rel).is_file() => citing.push(rel),
                None => {}
            }
        }
    }
    for rel in citing {
        let Ok(text) = std::fs::read_to_string(root.join(&rel)) else { continue };
        if replace_ids(&text, mapping) != text {
            plan.edits.push(rel);
        }
    }
    Ok(plan)
}

/// Applique le plan : corrige les contenus, renomme, puis écrit le journal data/migration-ids.json.
pub fn apply(root: &Path, mapping: &[(String, String)], plan: &Plan) -> Result<()> {
    for rel in &plan.edits {
        let path = root.join(rel);
        let text = std::fs::read_to_string(&path).with_context(|| format!("lecture de {}", path.display()))?;
        let tmp = path.with_extension("migration");
        std::fs::write(&tmp, replace_ids(&text, mapping))?;
        std::fs::rename(&tmp, &path)?;
    }
    for (from, to) in &plan.renames {
        std::fs::rename(root.join(from), root.join(to)).with_context(|| format!("renommage de {}", from.display()))?;
    }
    let journal = json!({"mapping": mapping, "renames": plan.renames, "edits": plan.edits});
    std::fs::write(root.join(JOURNAL), serde_json::to_string_pretty(&journal)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("bike360-migrate-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&root);
        for (file, text) in [
            ("data/cache/VID_20260829_112347.json", r#"{"id": "VID_20260829_112347", "parts": []}"#),
            ("data/cache/VID_20260829_112347_horizon.json", "{}"),
            ("data/cache/VID_20260829_112347_ZZ99.json", r#"{"id": "VID_20260829_112347_ZZ99"}"#),
            ("data/cache/privacy/VID_20260829_112347_fe798ca3_0_1.jpg", "x"),
            ("data/privacy/VID_20260829_112347.json", r#"{"fe798ca3": ["VID_20260829_112347_fe798ca3_0_1.jpg"]}"#),
            ("data/project.json", r#"{"sessions": ["VID_20260829_112347", "VID_20260829_112347_ZZ99", "VID_20260920_092259"]}"#),
            ("exports/VID_20260829_112347_final_1080p.mp4", "x"),
            ("exports/VID_20260829_112347/part.mp4", "x"),
        ] {
            let p = root.join(file);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        root
    }

    #[test]
    fn renames_files_and_fixes_contents_once() {
        let root = tree();
        let mapping = vec![("VID_20260829_112347".to_string(), "VID_20260829_112347_K7Q2".to_string())];
        let p = plan(&root, &mapping).unwrap();
        assert_eq!(p.renames.len(), 6, "{:?}", p.renames);
        assert!(p.renames.iter().all(|(from, _)| !from.to_string_lossy().contains("ZZ99")), "l'autre caméra n'est pas touchée");
        apply(&root, &mapping, &p).unwrap();
        let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap();
        assert_eq!(read("data/cache/VID_20260829_112347_K7Q2.json"), r#"{"id": "VID_20260829_112347_K7Q2", "parts": []}"#);
        assert_eq!(read("data/privacy/VID_20260829_112347_K7Q2.json"), r#"{"fe798ca3": ["VID_20260829_112347_K7Q2_fe798ca3_0_1.jpg"]}"#);
        assert_eq!(read("data/project.json"), r#"{"sessions": ["VID_20260829_112347_K7Q2", "VID_20260829_112347_ZZ99", "VID_20260920_092259"]}"#);
        assert!(root.join("data/cache/privacy/VID_20260829_112347_K7Q2_fe798ca3_0_1.jpg").exists());
        assert!(root.join("exports/VID_20260829_112347_K7Q2/part.mp4").exists());
        assert!(root.join("data/cache/VID_20260829_112347_ZZ99.json").exists());
        // une seconde passe ne trouve plus rien à faire
        assert_eq!(plan(&root, &mapping).unwrap(), Plan::default());
        let _ = std::fs::remove_dir_all(&root);
    }
}
