//! Outil de vérification : sorties JSON à comparer avec les modules Python.
use std::path::Path;

use anyhow::{bail, Context, Result};
use insta_core::{analyze, automontage, geometry, horizon, hyperlapse, insta360, paths};
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
        Some("analyze") => {
            // analyze DOSSIER... : comme server.load_sessions (dédoublonnage, blocs continus), sans cache
            for (s, r) in analyze::analyze_sessions(load_blocks(&args[2..]), true)? {
                eprintln!("{} : {:.1} min, GPS {:.0} %, décalage {:+.1} s ({}), {} candidats", s.id, r.duration as f64 / 60.0,
                          r.gps_coverage * 100.0, r.offset_s, r.offset_source, r.candidates.len());
            }
        }
        Some("horizon") => {
            // horizon SESSION DOSSIER... : horizon complet (cache de la racine) ; « range A B » en option via $RANGE
            let sid = &args[2];
            let all = analyze::analyze_sessions(load_blocks(&args[3..]), false)?;
            let (s, r) = all.iter().find(|(s, _)| &s.id == sid).context("session inconnue")?;
            let data = match std::env::var("RANGE").ok() {
                Some(range) => {
                    let (a, b) = range.split_once(',').context("RANGE=début,fin")?;
                    horizon::compute_range(s, r, a.parse()?, b.parse()?).context("portion trop courte")?
                }
                None => horizon::compute(s, r, &paths::cache(), Some(&|f| eprint!("\r{:.0} %", f * 100.0)))?,
            };
            println!("{}", serde_json::to_string(&data)?);
        }
        Some("emission") => {
            // emission IMAGE.gray : émissions processeur d'une image 1024×512 en niveaux de gris
            let img: Vec<f32> = std::fs::read(&args[2])?.iter().map(|v| *v as f32).collect();
            println!("{}", serde_json::to_string(&horizon::emission(&img, None))?);
        }
        Some("automontage") => {
            // automontage DURÉE CACHE.json... : plan sans clips existants (identifiants masqués)
            let mut results = std::collections::BTreeMap::new();
            for p in &args[3..] {
                let r: analyze::Analysis = serde_json::from_reader(std::fs::File::open(p)?)?;
                results.insert(r.id.clone(), r);
            }
            let mut plan = automontage::plan(&results, args[2].parse()?, &Default::default(), 0.6);
            plan.values_mut().flatten().for_each(|c| c.id = None);
            println!("{}", serde_json::to_string(&plan)?);
        }
        Some("hyperlapse") => {
            // hyperlapse CACHE.json DURÉE : résumé et instants des images
            let r: analyze::Analysis = serde_json::from_reader(std::fs::File::open(&args[2])?)?;
            let d: f64 = args[3].parse()?;
            println!("{}", json!({"summary": hyperlapse::summary(&r, d), "density": hyperlapse::density(&r, d),
                                  "times": hyperlapse::frame_times(&r, d)}));
        }
        _ => bail!("usage : insta-tool imu FICHIER | scan DOSSIER | views CLIPS.json PITCH ROLL | analyze DOSSIER... | horizon SESSION DOSSIER... | hyperlapse CACHE.json DURÉE"),
    }
    Ok(())
}

/// Sessions de plusieurs dossiers (première occurrence gardée), fusionnées en blocs continus.
fn load_blocks(dirs: &[String]) -> Vec<insta360::Session> {
    let mut sessions: Vec<insta360::Session> = vec![];
    for dir in dirs {
        for s in insta360::scan(Path::new(dir)) {
            if !sessions.iter().any(|x| x.id == s.id) {
                sessions.push(s);
            }
        }
    }
    let durations: std::collections::HashMap<String, f64> = sessions.iter()
        .map(|s| (s.id.clone(), s.segments.iter().filter_map(|x| x.lrv.as_ref()).map(|p| analyze::file_duration(p)).sum()))
        .collect();
    insta360::merge_continuous(sessions, |s| durations[&s.id])
}
