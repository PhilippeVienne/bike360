//! Sous-commandes de vérification du module de confidentialité (comparées à privacy.py).
//!
//! privacy-resize IN.bgr W H OW OH linear|area|hsv OUT : traitement d'image seul
//! privacy-detect RENDU.h264 N              : détections des N premières images (une sur deux)
//! privacy-analyze RENDU.h264 VUES.json SESSION CLIP : pistes (analyze_clip)
//! privacy-boxes PISTES.json CLIP VUES.json L H : zones par image de sortie (frame_boxes)
//! privacy-key SÉLECTIONS.json              : empreintes view_key des clips
//! privacy-follow RENDU.h264 TAILLE K0 X Y L H SENS FPS : suivi d'une zone (follow)
//! privacy-blur SOURCE SORTIE VUES.json PISTES.json CLIP L H [detect] : floutage ffmpeg (blur_video)
//! privacy-vit / privacy-match / privacy-link : suiveur, corrélation de gabarit, ancienne liaison
//!
//! VUES.json : {"times": [...], "mats": [[[3×3]]...], "fovs": [...]} (une entrée par image).
use std::path::Path;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use insta_core::geometry::Mat3;
use insta_core::privacy::{self, imgproc, Detector, FrameReader, Image};
use serde_json::{json, Value};

fn views(path: &str) -> Result<(Vec<f64>, Vec<Mat3>, Vec<f64>)> {
    let v: Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let times: Vec<f64> = serde_json::from_value(v["times"].clone())?;
    let mats: Vec<Mat3> = serde_json::from_value(v["mats"].clone())?;
    let fovs: Vec<f64> = serde_json::from_value(v["fovs"].clone())?;
    Ok((times, mats, fovs))
}

fn clip_tracks(path: &str, clip: &str) -> Result<Vec<privacy::Track>> {
    let data: privacy::SessionData = serde_json::from_reader(std::fs::File::open(path)?)?;
    Ok(privacy::all_tracks(data.get(clip)))
}

pub fn run(args: &[String]) -> Result<()> {
    let a = |i: usize| args.get(i).map(String::as_str).context("argument manquant");
    match args[1].as_str() {
        "privacy-resize" => {
            let (w, h, ow, oh): (usize, usize, usize, usize) = (a(3)?.parse()?, a(4)?.parse()?, a(5)?.parse()?, a(6)?.parse()?);
            let img = Image::from_bgr(w, h, std::fs::read(a(2)?)?);
            let out = match a(7)? {
                "linear" => imgproc::resize_linear(&img, ow, oh).data,
                "area" => imgproc::resize_area(&img, ow, oh).data,
                "hsv" => imgproc::light_mask(&img).into_iter().map(u8::from).collect(),
                m => bail!("mode inconnu {m}"),
            };
            std::fs::write(a(8)?, out)?;
        }
        "privacy-match" => {
            // privacy-match ZONE.bgr L H GABARIT.bgr L H : meilleure corrélation (TM_CCOEFF_NORMED)
            let area = Image::from_bgr(a(3)?.parse()?, a(4)?.parse()?, std::fs::read(a(2)?)?);
            let templ = Image::from_bgr(a(6)?.parse()?, a(7)?.parse()?, std::fs::read(a(5)?)?);
            println!("{}", serde_json::to_string(&imgproc::match_template_best(&area, &templ))?);
        }
        "privacy-detect" => {
            let n: usize = a(3)?.parse()?;
            let t0 = Instant::now();
            let mut det = Detector::new()?;
            let load = t0.elapsed().as_secs_f64();
            let mut r = FrameReader::open(Path::new(a(2)?), privacy::AW, privacy::AH)?;
            let (mut out, mut fi, mut t_det) = (vec![], 0usize, 0.0);
            while let Some(img) = r.next_frame() {
                if fi >= n {
                    break;
                }
                if fi % privacy::DETECT_EVERY == 0 {
                    let t = Instant::now();
                    let d = det.detect(&img, true)?;
                    t_det += t.elapsed().as_secs_f64();
                    out.push(json!([fi, d.iter().map(|d| json!([d.kind, d.conf, d.bbox])).collect::<Vec<_>>()]));
                }
                fi += 1;
            }
            eprintln!("chargement {load:.2} s, détection {t_det:.2} s pour {} images ({:.1} ms/image)", out.len(),
                      t_det * 1000.0 / out.len().max(1) as f64);
            println!("{}", serde_json::to_string(&out)?);
        }
        "privacy-analyze" => {
            let (times, mats, fovs) = views(a(3)?)?;
            let t0 = Instant::now();
            let mut det = Detector::new()?;
            let load = t0.elapsed().as_secs_f64();
            let t1 = Instant::now();
            let tracks = privacy::analyze_clip(Path::new(a(2)?), &times, &mats, &fovs, a(4)?, a(5)?, &mut det, None)?;
            eprintln!("chargement {load:.2} s, analyse {:.2} s ({} pistes)", t1.elapsed().as_secs_f64(), tracks.len());
            println!("{}", serde_json::to_string(&tracks)?);
        }
        "privacy-boxes" => {
            let tracks = clip_tracks(a(2)?, a(3)?)?;
            let (times, mats, fovs) = views(a(4)?)?;
            let b = privacy::frame_boxes(&times, &mats, &fovs, &tracks, a(5)?.parse()?, a(6)?.parse()?);
            println!("{}", serde_json::to_string(&b)?);
        }
        "privacy-roundtrip" => {
            // privacy-roundtrip SESSION : load puis save (racine $INSTA_BUILD_ROOT) : aucune perte attendue
            let data = privacy::load(a(2)?)?;
            privacy::save(a(2)?, &data)?;
            println!("{}", data.len());
        }
        "privacy-key" => {
            let clips: Vec<Value> = serde_json::from_reader(std::fs::File::open(a(2)?)?)?;
            let keys: Vec<String> = clips.iter().map(privacy::view_key).collect();
            println!("{}", serde_json::to_string(&keys)?);
        }
        "privacy-follow" => {
            let size: usize = a(3)?.parse()?;
            let k0: usize = a(4)?.parse()?;
            let b = [a(5)?.parse()?, a(6)?.parse()?, a(7)?.parse()?, a(8)?.parse()?];
            let dir: i64 = a(9)?.parse()?;
            let fps: f64 = a(10)?.parse()?;
            let frames = privacy::decode(Path::new(a(2)?), size, privacy::MANUAL_STEP)?;
            let mut net = privacy::nets::VitNet::new(&privacy::nets::project_model(privacy::nets::TRACK_MODEL)?, false)?;
            let t = Instant::now();
            let out = privacy::follow(&mut net, &frames, k0, b, dir, fps)?;
            eprintln!("suivi {:.2} s ({} images)", t.elapsed().as_secs_f64(), out.len());
            println!("{}", serde_json::to_string(&out)?);
        }
        "privacy-link" => {
            // privacy-link DÉTECTIONS.json : pistes écran (ancienne méthode) depuis [[image, [[type, conf, boîte]]]]
            let raw: Vec<(usize, Vec<(String, f64, privacy::BoxF)>)> = serde_json::from_reader(std::fs::File::open(a(2)?)?)?;
            let frames: Vec<(usize, Vec<privacy::Detection>)> = raw.into_iter()
                .map(|(f, d)| (f, d.into_iter()
                    .map(|(k, conf, bbox)| privacy::Detection { kind: if k == "visage" { "visage" } else { "plaque" }, conf, bbox })
                    .collect()))
                .collect();
            println!("{}", serde_json::to_string(&privacy::link(&frames))?);
        }
        "privacy-zone" => {
            // privacy-zone RENDU_LOCAL.h264 TAILLE T_DÉBUT FPS T0 DX DY DZ AX AY : zone manuelle suivie
            // (vue locale regardant vers d, champ local_fov) → échantillons ; + zone fixe [T0, T0+2]
            let size: usize = a(3)?.parse()?;
            let (start, fps, t0): (f64, f64, f64) = (a(4)?.parse()?, a(5)?.parse()?, a(6)?.parse()?);
            let d = [a(7)?.parse()?, a(8)?.parse()?, a(9)?.parse()?];
            let (ax, ay): (f64, f64) = (a(10)?.parse()?, a(11)?.parse()?);
            let frames = privacy::decode(Path::new(a(2)?), size, privacy::MANUAL_STEP)?;
            let times: Vec<f64> = (0..frames.len()).map(|i| start + (i * privacy::MANUAL_STEP) as f64 / fps).collect();
            let (m, fov) = (privacy::local_view(&d), privacy::local_fov(ax, ay));
            let mut net = privacy::nets::VitNet::new(&privacy::nets::project_model(privacy::nets::TRACK_MODEL)?, false)?;
            let (samples, thumb) = privacy::track_manual_zone(&mut net, &frames, &times, fps, t0, &d, ax, ay, &m, fov)?;
            let fixed = privacy::fixed_zone_samples(t0, t0 + 2.0, &d, ax, ay);
            println!("{}", json!({"samples": samples, "thumb": thumb.map(|t| [t.w, t.h]), "fixed": fixed}));
        }
        "privacy-vit" => {
            // privacy-vit RENDU.h264 L H K0 X Y BL BH N [cpu] : suiveur seul, N mises à jour depuis K0
            let (w, h): (usize, usize) = (a(3)?.parse()?, a(4)?.parse()?);
            let k0: usize = a(5)?.parse()?;
            let b = [a(6)?.parse()?, a(7)?.parse()?, a(8)?.parse()?, a(9)?.parse()?];
            let n: usize = a(10)?.parse()?;
            let gpu = args.get(11).map(String::as_str) != Some("cpu");
            let mut net = privacy::nets::VitNet::new(&privacy::nets::project_model(privacy::nets::TRACK_MODEL)?, gpu)?;
            let mut r = FrameReader::open(Path::new(a(2)?), w, h)?;
            let step: usize = std::env::var("STEP").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
            let (mut fi, mut tr, mut out, mut dt, mut raw) = (0usize, None, vec![], 0.0, 0usize);
            while let Some(img) = r.next_frame() {
                raw += 1;
                if (raw - 1) % step != 0 {
                    continue;
                }
                if fi == k0 {
                    tr = Some(privacy::nets::VitTracker::init(&img, b));
                } else if fi > k0 && fi <= k0 + n {
                    let t = tr.as_mut().unwrap();
                    let t0 = Instant::now();
                    let (ok, bb) = t.update(&mut net, &img)?;
                    dt += t0.elapsed().as_secs_f64();
                    out.push(json!([ok, bb, t.score]));
                } else if fi > k0 + n {
                    break;
                }
                fi += 1;
            }
            eprintln!("{:.2} ms par mise à jour", dt * 1000.0 / n as f64);
            println!("{}", serde_json::to_string(&out)?);
        }
        "privacy-blur" => {
            let (times, mats, fovs) = views(a(4)?)?;
            let tracks = clip_tracks(a(5)?, a(6)?)?;
            // 9e argument « detect » : détection directe par image (résumé hyperlapse)
            let mut det = if args.get(9).map(String::as_str) == Some("detect") { Some(Detector::new()?) } else { None };
            // encodeur : $PRIVACY_ENC (ex. « -c:v libx264rgb -qp 0 » pour comparer sans perte)
            let enc: Vec<String> = std::env::var("PRIVACY_ENC").unwrap_or_else(|_| "-c:v libx264 -crf 20".into())
                .split_whitespace().map(String::from).collect();
            let t = Instant::now();
            let n = privacy::blur_video(Path::new(a(2)?), Path::new(a(3)?), &times, &mats, &fovs, &tracks, a(7)?.parse()?,
                                        a(8)?.parse()?, &enc, det.as_mut(), None)?;
            eprintln!("floutage {:.2} s", t.elapsed().as_secs_f64());
            println!("{n}");
        }
        c => bail!("sous-commande inconnue {c}"),
    }
    Ok(())
}
