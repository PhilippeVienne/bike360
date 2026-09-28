//! Compile les noyaux CUDA en PTX (chargé à l'exécution par cudarc).
use std::path::PathBuf;
use std::process::Command;

fn main() {
    for name in ["reproject", "horizon"] {
        println!("cargo:rerun-if-changed=src/{name}.cu");
        let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join(format!("{name}.ptx"));
        let status = Command::new("nvcc")
            .args(["--ptx", "-O3", "--use_fast_math", "-arch=compute_75", &format!("src/{name}.cu"), "-o"])
            .arg(&out)
            .status()
            .expect("nvcc introuvable (kit CUDA requis)");
        assert!(status.success(), "échec de compilation du noyau CUDA {name}");
    }
}
