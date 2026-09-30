//! Outil de vérification : sorties JSON à comparer avec les modules Python.
use std::path::Path;

use anyhow::{bail, Result};
use insta_core::insta360;
use serde_json::json;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("imu") => {
            let imu = insta360::read_imu(Path::new(&args[2]))?;
            let n = imu.t.len() as f64;
            let mean = |v: &[[f64; 3]]| [0, 1, 2].map(|k| v.iter().map(|x| x[k]).sum::<f64>() / n);
            println!("{}", json!({"n": imu.t.len(), "t0": imu.t[0], "t_last": imu.t[imu.t.len() - 1],
                                  "acc_mean": mean(&imu.acc), "gyro_mean": mean(&imu.gyro)}));
        }
        Some("scan") => println!("{}", serde_json::to_string(&insta360::scan(Path::new(&args[2])))?),
        _ => bail!("usage : insta-tool imu FICHIER | scan DOSSIER"),
    }
    Ok(())
}
