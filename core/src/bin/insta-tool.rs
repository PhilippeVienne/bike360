//! Outil de vérification : sorties JSON à comparer avec les modules Python.
use std::path::Path;

use anyhow::{bail, Result};
use insta_core::{geometry, insta360};
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
        Some("views") => {
            // views CLIPS.json TILT_PITCH TILT_ROLL : cadrage, matrice et angles v360 à 40 instants par clip
            let clips: Vec<geometry::Clip> = serde_json::from_reader(std::fs::File::open(&args[2])?)?;
            let tilt = geometry::Tilt { pitch: args[3].parse()?, roll: args[4].parse()? };
            let l = geometry::tilt_matrix(Some(&tilt));
            let mut out = vec![];
            for c in &clips {
                for k in 0..40 {
                    let t = (c.end - c.start) * k as f64 / 39.0;
                    let v = geometry::clip_view_at(c, t);
                    let m = geometry::view_matrix(v.yaw, v.pitch, Some(&l), v.roll);
                    out.push(json!({"v": [v.yaw, v.pitch, v.roll, v.fov], "m": m, "a": geometry::v360_angles(&m)}));
                }
            }
            println!("{}", serde_json::to_string(&out)?);
        }
        _ => bail!("usage : insta-tool imu FICHIER | scan DOSSIER | views CLIPS.json PITCH ROLL"),
    }
    Ok(())
}
