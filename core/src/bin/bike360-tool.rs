//! Outil de vérification : sorties JSON à comparer avec les modules Python.
use std::path::Path;

use anyhow::{bail, Context, Result};
use bike360_core::{analyze, arrival, automontage, basemap, endcard, finishing, geometry, horizon, hyperlapse, insta360, lean, migrate, musiclib, paths, telemetry};
use serde_json::{json, Value};

#[path = "bike360-tool/privacy.rs"]
mod privacy_tool;

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
        Some("migrate-ids") => migrate_ids(&args[2..])?,
        // arrivee DOSSIER_RUSHS DOSSIER_VIGNETTES : analyse d'arrivée (service hébergé), résumé JSON sur la sortie
        Some("arrivee") => println!("{}", serde_json::to_string(&arrival::run(Path::new(&args[2]), Path::new(&args[3]))?)?),
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
        Some("music") => {
            // music REQUÊTE AMBIANCE : recherche dans le catalogue en cache
            println!("{}", serde_json::to_string(&musiclib::search(&args[2], &args[3], 60, 60)?)?);
        }
        Some("lean") => {
            // lean CACHE.json HORIZON.json : série d'angle (10 Hz), angle GPS et statistiques
            let r: analyze::Analysis = serde_json::from_reader(std::fs::File::open(&args[2])?)?;
            let h: horizon::HorizonData = serde_json::from_reader(std::fs::File::open(&args[3])?)?;
            let series = lean::lean_series(&h, &r.tilt);
            let (_, gps_lean) = horizon::gps_prior_inputs(&r, series.len());
            println!("{}", json!({"lean": series, "gps": gps_lean, "stats": lean::lean_stats(&h, &r)}));
        }
        Some("hyperlapse") => {
            // hyperlapse CACHE.json DURÉE : résumé et instants des images
            let r: analyze::Analysis = serde_json::from_reader(std::fs::File::open(&args[2])?)?;
            let d: f64 = args[3].parse()?;
            println!("{}", json!({"summary": hyperlapse::summary(&r, d), "density": hyperlapse::density(&r, d),
                                  "times": hyperlapse::frame_times(&r, d)}));
        }
        Some(cmd @ ("basemap" | "mappanel" | "layers" | "overlay" | "endcard" | "finishing")) => {
            // CMD SPEC.json : sorties à comparer avec la version Python (voir les champs lus ci-dessous)
            let spec: Value = serde_json::from_reader(std::fs::File::open(&args[2])?)?;
            println!("{}", serde_json::to_string(&port_check(cmd, &spec)?)?);
        }
        Some(c) if c.starts_with("privacy-") => privacy_tool::run(&args)?,
        _ => bail!("usage : bike360-tool imu FICHIER | scan DOSSIER | views CLIPS.json PITCH ROLL | analyze DOSSIER... | horizon SESSION DOSSIER... | hyperlapse CACHE.json DURÉE | lean CACHE.json HORIZON.json | basemap|mappanel|layers|overlay|endcard|finishing SPEC.json | privacy-… (voir bike360-tool/privacy.rs)"),
    }
    Ok(())
}

/// Sessions de plusieurs dossiers (première occurrence gardée), fusionnées en blocs continus.
/// migrate-ids [--apply] [DOSSIER...] : passe les données de la racine aux identifiants de session
/// avec caméra. Les dossiers donnés s'ajoutent à ceux de data/sources.json ; sans --apply, rien n'est écrit.
fn migrate_ids(args: &[String]) -> Result<()> {
    let apply = args.iter().any(|a| a == "--apply");
    let mut dirs: Vec<String> = std::fs::read_to_string(paths::data().join("sources.json")).ok()
        .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    dirs.extend(args.iter().filter(|a| *a != "--apply").cloned());
    // sessions dont les fichiers sont lisibles : identifiant sans caméra → nouveaux identifiants
    let mut seen: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = Default::default();
    let mut suffixes = std::collections::BTreeSet::new();
    for dir in &dirs {
        for s in insta360::scan(Path::new(dir)) {
            if let Some(c) = &s.camera {
                suffixes.insert(insta360::camera_suffix(&c.serial));
                seen.entry(insta360::bare_id(&s.id).to_string()).or_default().insert(s.id.clone());
            }
        }
    }
    let root = paths::root();
    if paths::data() != root.join("data") || paths::exports() != root.join("exports") {
        bail!("migrate-ids ne gère que la disposition par défaut (data/ et exports/ sous la racine)");
    }
    let (mut mapping, mut assumed, mut skipped) = (vec![], 0, vec![]);
    for old in migrate::old_ids(&root) {
        match seen.get(&old).map(|ids| ids.iter().collect::<Vec<_>>()).as_deref() {
            Some([new]) => mapping.push((old, (*new).clone())),
            Some(_) => skipped.push(format!("{old} : filmée par plusieurs caméras, à départager à la main")),
            // fichiers absents (carte retirée) : une seule caméra connue, on la suppose
            None if suffixes.len() == 1 => {
                assumed += 1;
                mapping.push((old.clone(), format!("{old}_{}", suffixes.first().unwrap())));
            }
            None => skipped.push(format!("{old} : fichiers absents et caméra inconnue")),
        }
    }
    let plan = migrate::plan(&root, &mapping)?;
    println!("Racine : {}", root.display());
    println!("Dossiers lus : {}", if dirs.is_empty() { "aucun".into() } else { dirs.join(", ") });
    println!("Caméras reconnues : {}", suffixes.len());
    println!("Sessions à renommer : {} (dont {} dont les fichiers sont absents, caméra supposée)", mapping.len(), assumed);
    for (old, new) in &mapping {
        println!("  {old} → {new}");
    }
    for s in &skipped {
        println!("  laissée telle quelle : {s}");
    }
    println!("Fichiers et dossiers à renommer : {}", plan.renames.len());
    println!("Fichiers JSON à corriger : {}", plan.edits.len());
    for f in &plan.edits {
        println!("  {}", f.display());
    }
    if !apply {
        println!("Essai à blanc : rien n'a été écrit. Relancer avec --apply, serveur arrêté, pour migrer.");
        return Ok(());
    }
    migrate::apply(&root, &mapping, &plan)?;
    println!("Migration faite ; journal dans data/migration-ids.json.");
    Ok(())
}

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

fn load(path: &Value) -> Result<analyze::Analysis> {
    let p = path.as_str().context("chemin d'analyse")?;
    Ok(serde_json::from_reader(std::fs::File::open(p).with_context(|| p.to_string())?)?)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Vérifications des modules de rendu (basemap, telemetry, endcard, finishing).
fn port_check(cmd: &str, spec: &Value) -> Result<Value> {
    let f = |k: &str| spec[k].as_f64().unwrap_or(0.0);
    let results: Vec<analyze::Analysis> = spec["caches"].as_array().map(|a| a.iter().map(load).collect()).transpose()?.unwrap_or_default();
    let refs: Vec<&analyze::Analysis> = results.iter().collect();
    let out = |k: &str| Path::new(spec[k].as_str().unwrap_or_default()).to_path_buf();
    Ok(match cmd {
        "basemap" => {
            let lats: Vec<f64> = refs.iter().flat_map(|r| telemetry::raw(r, "lat")).collect();
            let lons: Vec<f64> = refs.iter().flat_map(|r| telemetry::raw(r, "lon")).collect();
            match basemap::render(&lats, &lons, f("size") as u32, f("pad"), f("max_fill")) {
                None => Value::Null,
                Some((img, p)) => {
                    img.save(out("out"))?;
                    let pts: Vec<(f64, f64)> = (0..lats.len()).step_by(37).map(|i| p.project(lats[i], lons[i])).collect();
                    json!({"points": pts})
                }
            }
        }
        "mappanel" => {
            let r = load(&spec["result"])?;
            let clip = telemetry::Span { start: f("start"), end: f("end") };
            let (panel, p, attribution) = telemetry::map_panel(&r, clip, &refs, f("S") as usize, f("U"));
            panel.save_png(&out("out"))?;
            let (lat, lon) = (telemetry::raw(&r, "lat"), telemetry::raw(&r, "lon"));
            let pts: Vec<(f64, f64)> = (0..lat.len()).step_by(37).map(|i| p.project(lat[i], lon[i])).collect();
            json!({"attribution": attribution, "points": pts})
        }
        "layers" | "overlay" => {
            let r = load(&spec["result"])?;
            let clip = telemetry::Span { start: f("start"), end: f("end") };
            let opts = telemetry::Options::merged(spec["opts"].as_object());
            let (t0, speedup) = (f("t0"), f("speedup"));
            let tm = move |t: f64| t0 + speedup * t;
            let time_map: Option<&dyn Fn(f64) -> f64> = if speedup > 0.0 { Some(&tm) } else { None };
            let (w, h) = (f("W") as usize, f("H") as usize);
            let first = spec["first_part"].as_bool().unwrap_or(false);
            if cmd == "layers" {
                // « horizon » (facultatif) : cache <session>_horizon.json → jauge d'inclinaison
                let track = match spec["horizon"].as_str() {
                    Some(p) => {
                        let hd: horizon::HorizonData = serde_json::from_reader(std::fs::File::open(p)?)?;
                        Some(lean::LeanTrack::new(&hd, &r.tilt))
                    }
                    None => None,
                };
                let l = telemetry::layers(&r, clip, t0, f("n_frames") as usize, f("fps"), w, h, &opts, first, &out("workdir"),
                                          time_map, &refs, track.as_ref())?;
                serde_json::to_value(l)?
            } else {
                let c = telemetry::overlay_command(&out("part"), &out("out"), &r, clip, t0, f("dur"), w, h, &opts, first,
                                                   &strings(&spec["encoder_args"]), &out("workdir"), time_map, &refs)?;
                json!({"cmd": c, "cmdfile": std::fs::read_to_string(out("workdir").join("telemetry.cmd")).ok()})
            }
        }
        "endcard" => {
            let credits = strings(&spec["credits"]);
            let s = endcard::render(&refs, f("W") as usize, f("H") as usize, &out("out"), spec["title"].as_str().unwrap_or(""), &credits,
                                  &serde_json::from_value::<Vec<lean::LeanStats>>(spec["lean"].clone()).unwrap_or_default())?;
            json!({"summary": s, "label": endcard::date_label(&s.days)})
        }
        _ => {   // finishing
            let style = finishing::clean(spec.get("style"))?;
            let clips: Vec<(String, f64, bool)> = serde_json::from_value(spec["clips"].clone())?;
            let files: Vec<&Path> = clips.iter().map(|c| Path::new(&c.0)).collect();
            let info: Vec<(f64, bool)> = clips.iter().map(|c| (c.1, c.2)).collect();
            let opt = |k: &str| spec[k].as_str().map(Path::new);
            // fichiers audio : {nom du fichier → [chemin, durée]}
            let tracks: Vec<finishing::Track> = style["audio_tracks"].as_array().into_iter().flatten()
                .filter_map(|t| {
                    let f = &spec["audio_files"][t["file"].as_str()?];
                    Some(finishing::Track { path: f[0].as_str()?.into(), seconds: f[1].as_f64()?, spec: t })
                })
                .collect();
            let (cmd, total) = finishing::finish_command(&files, &info, &out("final"), &style, &strings(&spec["encoder_args"]),
                                                         f("W") as usize, f("H") as usize, &tracks,
                                                         spec["audio_bitrate"].as_str().unwrap_or("160k"), opt("end_card"),
                                                         &strings(&spec["credits"]));
            json!({"clean": style, "plain": finishing::is_plain(&style), "cmd": cmd, "total": total})
        }
    })
}
