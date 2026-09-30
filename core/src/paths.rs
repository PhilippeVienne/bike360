//! Emplacements des données (même arborescence que la version Python).
use std::path::PathBuf;

/// Racine du projet : $BIKE360_ROOT, sinon le dossier du dépôt compilé.
pub fn root() -> PathBuf {
    std::env::var_os("BIKE360_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")))
}

pub fn data() -> PathBuf {
    root().join("data")
}

pub fn cache() -> PathBuf {
    data().join("cache")
}
