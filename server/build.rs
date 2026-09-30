//! Embarque l'interface web (ui/) dans le binaire : liste des fichiers et `include_bytes!`
//! générés dans $OUT_DIR/ui_files.rs. Les fichiers présents sur disque restent prioritaires.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            println!("cargo:rerun-if-changed={}", p.display());
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui").canonicalize().unwrap_or_default();
    println!("cargo:rerun-if-changed={}", ui.display());
    let mut files = vec![];
    walk(&ui, &mut files);
    let mut code = String::from("/// Fichiers de ui/ embarqués : (chemin relatif, contenu).\npub static UI_FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let rel = f.strip_prefix(&ui).unwrap().to_string_lossy().replace('\\', "/");
        code += &format!("    ({rel:?}, include_bytes!({:?})),\n", f.display().to_string());
    }
    code += "];\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ui_files.rs");
    std::fs::write(out, code).unwrap();
}
