//! Confidentialité côté serveur : orchestration autour de `bike360_core::privacy` (portage des
//! fonctions de server.py qui l'utilisent).
//!
//! - analyse des clips dans leur cadrage : rendu GPU en 1920×1080, détection et suivi, pistes
//!   enregistrées dans data/privacy/ (`analyze`) ;
//! - zone tracée à la main, fixe ou suivie dans une vue locale (`manual_zone`) ;
//! - suivi d'un compagnon sur la sphère, par vues locales successives (`track_sphere`) ;
//! - floutage à l'export (`frame_boxes` pour le moteur GPU, `blur_video` sinon).
//!
//! Un seul détecteur (modèles ONNX, sur la carte graphique) est chargé à la première analyse
//! puis réutilisé ; de même pour le réseau de suivi des zones tracées (VitTrack, processeur).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::{bail, Context, Result};
use bike360_core::geometry::{self, Mat3};
use bike360_core::insta360::Session;
use bike360_core::analyze::Analysis;
use bike360_core::paths;
use bike360_core::privacy::nets::{self, VitNet};
use bike360_core::privacy::{self, Detector, Image, SessionData, Track};
use serde_json::{json, Map, Value};

use crate::app::{render_bin, App, Job};
use crate::export::{clip_parts, flat, part_targets, run_part_process, seg_offset, source_fps, to_clip};
use crate::pyjson;

// ---------------------------------------------------------------- modèles partagés

static DETECTOR: Mutex<Option<Detector>> = Mutex::new(None);
static TRACKER: Mutex<Option<VitNet>> = Mutex::new(None);

fn lock<T>(m: &'static Mutex<T>) -> MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Détecteur partagé (chargé à la première utilisation). Le verrou est tenu pendant `f` : deux
/// tâches qui détectent en même temps passent l'une après l'autre.
fn with_detector<R>(f: impl FnOnce(&mut Detector) -> Result<R>) -> Result<R> {
    let mut g = lock(&DETECTOR);
    if g.is_none() {
        *g = Some(Detector::new()?);
    }
    f(g.as_mut().unwrap())
}

/// Réseau de suivi des zones tracées à la main (chargé à la première utilisation).
fn with_tracker<R>(f: impl FnOnce(&mut VitNet) -> Result<R>) -> Result<R> {
    let mut g = lock(&TRACKER);
    if g.is_none() {
        *g = Some(VitNet::new(&nets::project_model(nets::TRACK_MODEL)?, false)?);
    }
    f(g.as_mut().unwrap())
}

/// Erreur d'annulation, à retourner depuis les rappels de progression.
fn check_cancel(job: &Job) -> Result<()> {
    if job.is_cancelled() {
        bail!("annulé");
    }
    Ok(())
}

// ---------------------------------------------------------------- fichiers

/// Zones d'une session (vide si le fichier est absent ou illisible) — pour la lecture seule.
/// Pour modifier puis réécrire, utiliser `privacy::load` (une erreur ne doit pas effacer le fichier).
pub fn load(sid: &str) -> SessionData {
    privacy::load(sid).unwrap_or_else(|e| {
        eprintln!("Confidentialité : {e:#}");
        SessionData::new()
    })
}

/// Pistes détectées + zones tracées d'un clip (identifiant JSON brut de la sélection).
pub fn clip_tracks(sid: &str, clip_id: Option<&Value>) -> Vec<Track> {
    let data = load(sid);
    privacy::all_tracks(clip_id.and_then(|id| data.get(&pyjson::py_str(id))))
}

/// Clips jamais analysés, ou dont le cadrage a changé depuis l'analyse.
pub fn pending(clips: &[(String, Map<String, Value>)]) -> Vec<(String, Map<String, Value>)> {
    clips.iter()
        .filter(|(sid, c)| !privacy::is_analyzed(&load(sid), &Value::Object(c.clone())))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------- floutage à l'export

/// Zones à flouter par image de sortie [[x, y, w, h]] pour le moteur GPU.
pub fn frame_boxes(times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Track], w: u32, h: u32) -> Value {
    json!(privacy::frame_boxes(times, mats, fovs, tracks, w as usize, h as usize))
}

/// Floute les zones des pistes dans `src` → `dst` (ffmpeg) ; `detect` : détection image par
/// image en plus (hyperlapse). `progress(f)` de 0 à 1 ; annulable par la tâche.
#[allow(clippy::too_many_arguments)]
pub fn blur_video(src: &Path, dst: &Path, times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Track], w: u32, h: u32,
                  encoder_args: &[String], detect: bool, job: &Job, progress: impl Fn(f64)) -> Result<()> {
    let p = |f: f64| -> Result<()> {
        check_cancel(job)?;
        progress(f);
        Ok(())
    };
    let run = |det: Option<&mut Detector>| {
        privacy::blur_video(src, dst, times, mats, fovs, tracks, w as usize, h as usize, encoder_args, det, Some(&p))
    };
    if detect {
        with_detector(|d| run(Some(d)))?;
    } else {
        run(None)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- analyse des clips

fn work_dir() -> Result<PathBuf> {
    let d = paths::cache().join("privacy_render");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Rendu GPU d'un travail (fichier JSON du moteur) ; le processus est rattaché à la tâche.
fn render(job: &Job, spec_path: &Path, spec: &Value) -> Result<()> {
    std::fs::write(spec_path, serde_json::to_string(spec)?)?;
    run_part_process(job, &[render_bin().display().to_string(), spec_path.display().to_string()], |_| {})
}

/// Analyse de confidentialité (visages et plaques) de clips dans leur cadrage : rendu GPU,
/// détection, pistes enregistrées dans data/privacy/. Retourne (zones trouvées, clips déjà à jour).
/// `label` préfixe les messages (analyse lancée par un export).
pub fn analyze(app: &App, job: &Job, items: &[(String, Map<String, Value>)], force: bool, label: &str)
               -> Result<(u64, u64)> {
    if !app.gpu_engine_available() {
        bail!("l'analyse demande le moteur GPU (render/ + NVENC)");
    }
    job.set("message", format!("{label}chargement des modèles"));
    with_detector(|_| Ok(()))?;
    let work = work_dir()?;
    let cstart = |c: &Map<String, Value>| c.get("start").and_then(Value::as_f64).unwrap_or(0.0);
    let cend = |c: &Map<String, Value>| c.get("end").and_then(Value::as_f64).unwrap_or(0.0);
    let total: f64 = items.iter().map(|(_, c)| cend(c) - cstart(c)).sum();
    let total = if total == 0.0 { 1.0 } else { total };
    let (mut done, mut found, mut skipped) = (0.0, 0u64, 0u64);
    for (n, (sid, raw)) in items.iter().enumerate() {
        check_cancel(job)?;
        let raw_v = Value::Object(raw.clone());
        let clip_id = raw.get("id").map(pyjson::py_str).unwrap_or_default();
        if !force && privacy::is_analyzed(&load(sid), &raw_v) {
            done += cend(raw) - cstart(raw);
            skipped += 1;
            continue;
        }
        let clip = to_clip(raw)?;
        let sess = app.sess(sid).with_context(|| format!("session inconnue : {sid}"))?;
        let (session, result) = (&sess.session, &sess.result);
        let full = app.horizon_done(sid);
        let range;
        let mut hdata = full.as_deref();
        if geometry::clip_horizon_mode(&clip) == geometry::HorizonMode::Auto && hdata.is_none() {
            range = bike360_core::horizon::compute_range(session, result, clip.start - 3.0, clip.end + 3.0);
            hdata = range.as_ref();
        }
        let mut tracks: Vec<Track> = vec![];
        for (k, (seg, ss, dur)) in clip_parts(session, result, clip.start, clip.end).into_iter().enumerate() {
            let Some(insv) = &seg.insv else { bail!("fichier .insv manquant pour {sid}") };
            let (_, fps) = source_fps(insv)?;
            let off = seg_offset(result, seg)?;
            let targets = part_targets(&clip, result, hdata, off, ss, dur, 1.0 / fps);
            let stem = format!("{sid}_{clip_id}_{k}");
            let (h264, spec) = (work.join(format!("{stem}.h264")), work.join(format!("{stem}.json")));
            job.set("message", format!("{label}clip {}/{} : rendu", n + 1, items.len()));
            let times: Vec<f64> = targets.iter().map(|x| off + ss + x.0).collect();
            let mats: Vec<Mat3> = targets.iter().map(|x| x.1).collect();
            let fovs: Vec<f64> = targets.iter().map(|x| x.2).collect();
            let base = done;
            let progress = |f: f64| -> Result<()> {
                check_cancel(job)?;
                job.set("progress", ((base + f * dur) / total).min(1.0));
                Ok(())
            };
            let part = render(job, &spec, &json!({
                "source": insv, "start": ss, "duration": dur, "width": privacy::AW, "height": privacy::AH,
                "fov": fovs[0], "fovs": fovs, "cq": 21, "masks": [], "matrices": mats.iter().map(flat).collect::<Vec<_>>(),
                "output": h264}))
                .and_then(|_| {
                    job.set("message", format!("{label}clip {}/{} : détection", n + 1, items.len()));
                    with_detector(|det| {
                        privacy::analyze_clip(&h264, &times, &mats, &fovs, sid, &format!("{clip_id}_{k}"), det,
                                              Some(&progress))
                    })
                });
            // fichiers de travail supprimés même en cas d'échec ou d'annulation
            let _ = std::fs::remove_file(&h264);
            let _ = std::fs::remove_file(&spec);
            for mut t in part? {
                t.id = Value::from(tracks.len());
                tracks.push(t);
            }
            done += dur;
        }
        found += tracks.len() as u64;
        let mut data = privacy::load(sid)?;
        let entry = data.entry(clip_id).or_default();
        entry.key = Some(privacy::view_key(&raw_v));
        entry.tracks = Some(tracks);
        privacy::save(sid, &data)?;
    }
    Ok((found, skipped))
}

// ---------------------------------------------------------------- vues locales (zones tracées, suivi)

static LOCAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Rendu (moteur GPU) d'une vue carrée fixe `m` entre a et b, une image sur MANUAL_STEP :
/// (images, instants de session, cadence de la source).
#[allow(clippy::too_many_arguments)]
fn render_local(job: &Job, session: &Session, result: &Analysis, a: f64, b: f64, m: &Mat3, fov: f64, size: usize)
                -> Result<(Vec<Image>, Vec<f64>, f64)> {
    let work = work_dir()?;
    let (mut frames, mut times, mut fps) = (vec![], vec![], 30000.0 / 1001.0);
    // nom propre à chaque rendu : plusieurs tâches peuvent tourner en même temps
    let tag = format!("local_{}_{}", std::process::id(), LOCAL_SEQ.fetch_add(1, Ordering::SeqCst));
    for (k, (seg, ss, dur)) in clip_parts(session, result, a, b).into_iter().enumerate() {
        let Some(insv) = &seg.insv else { bail!("fichier .insv manquant") };
        fps = source_fps(insv)?.1;
        let off = seg_offset(result, seg)?;
        let (h264, spec) = (work.join(format!("{tag}_{k}.h264")), work.join(format!("{tag}_{k}.json")));
        let r = render(job, &spec, &json!({
            "source": insv, "start": ss, "duration": dur, "width": size, "height": size, "fov": fov, "fovs": [fov],
            "cq": 23, "masks": [], "matrices": [flat(m)], "output": h264}))
            .and_then(|_| privacy::decode(&h264, size, privacy::MANUAL_STEP));
        let _ = std::fs::remove_file(&h264);
        let _ = std::fs::remove_file(&spec);
        let part = r?;
        times.extend((0..part.len()).map(|i| off + ss + (i * privacy::MANUAL_STEP) as f64 / fps));
        frames.extend(part);
    }
    Ok((frames, times, fps))
}

fn normalized(d: [f64; 3]) -> [f64; 3] {
    let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    d.map(|v| v / n)
}

fn nearest(times: &[f64], t: f64) -> usize {
    let mut k0 = 0;
    for (k, x) in times.iter().enumerate() {
        if (x - t).abs() < (times[k0] - t).abs() {
            k0 = k;
        }
    }
    k0
}

/// Six caractères hexadécimaux aléatoires (`secrets.token_hex(3)`).
fn token_hex3() -> String {
    bike360_core::automontage::new_id()[..6].to_string()
}

/// Zone tracée à la main à l'instant t0 : suivie dans le temps (VitTrack) ou fixe sur le clip
/// (élément solidaire de la moto) ; enregistrée dans data/privacy/. Retourne le message de fin.
#[allow(clippy::too_many_arguments)]
pub fn manual_zone(app: &App, job: &Job, sid: &str, raw: &Map<String, Value>, t0: f64, d0: [f64; 3], ax: f64, ay: f64,
                   track_it: bool) -> Result<String> {
    let clip = to_clip(raw)?;
    let clip_id = raw.get("id").map(pyjson::py_str).unwrap_or_default();
    let d0 = normalized(d0);
    let mut thumb = None;
    let samples = if !track_it {
        privacy::fixed_zone_samples(clip.start, clip.end, &d0, ax, ay)
    } else {
        if !app.gpu_engine_available() {
            bail!("le suivi demande le moteur GPU (render/ + NVENC)");
        }
        let sess = app.sess(sid).context("session inconnue")?;
        let (size, fov) = (privacy::MANUAL_SIZE, privacy::local_fov(ax, ay));
        let m = privacy::local_view(&d0);
        let a = clip.start.max(t0 - privacy::MANUAL_WINDOW_S);
        let b = clip.end.min(t0 + privacy::MANUAL_WINDOW_S);
        job.set("message", "rendu autour de la zone");
        let (frames, times, fps) = render_local(job, &sess.session, &sess.result, a, b, &m, fov, size)?;
        if frames.is_empty() {
            bail!("aucune image rendue autour de la zone");
        }
        job.set("message", "suivi de la zone");
        let (samples, img) =
            with_tracker(|net| privacy::track_manual_zone(net, &frames, &times, fps, t0, &d0, ax, ay, &m, fov))?;
        if let Some(img) = img {
            let name = format!("{sid}_{clip_id}_manuel_{}.jpg", token_hex3());
            std::fs::create_dir_all(privacy::thumbs_dir())?;
            privacy::write_jpeg(&img, &privacy::thumbs_dir().join(&name))?;
            thumb = Some(name);
        }
        samples
    };
    let span = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) => b[0] - a[0],
        _ => 0.0,
    };
    let mut data = privacy::load(sid)?;
    let entry = data.entry(clip_id).or_default();
    entry.manual.get_or_insert_with(Vec::new).push(Track {
        id: Value::String(format!("m{}", token_hex3())),
        kind: if track_it { "manuel" } else { "fixe" }.into(),
        conf: 1.0,
        enabled: Some(true),
        thumb,
        samples,
        thumb_conf: 0.0,
        extra: Map::new(),
    });
    privacy::save(sid, &data)?;
    Ok(format!("zone {} sur {span:.1} s", if track_it { "suivie" } else { "fixe" }))
}

const FOLLOW_CHUNK_S: f64 = 3.0; // suivi d'un compagnon : vue locale recentrée toutes les 3 s

/// Instant de session utilisable comme clé triée.
#[derive(Clone, Copy, PartialEq)]
struct T(f64);
impl Eq for T {}
impl PartialOrd for T {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for T {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0)
    }
}

/// Suit un objet sur la sphère entre a et b depuis t0 (vues locales rendues par le moteur GPU,
/// recentrées toutes les FOLLOW_CHUNK_S, suivi VitTrack) : [(t, direction)] triés.
#[allow(clippy::too_many_arguments)]
pub fn track_sphere(app: &App, job: &Job, sid: &str, t0: f64, d0: [f64; 3], ax: f64, ay: f64, a: f64, b: f64)
                    -> Result<Vec<(f64, [f64; 3])>> {
    let sess = app.sess(sid).context("session inconnue")?;
    let (size, fov) = (privacy::MANUAL_SIZE, privacy::local_fov(ax, ay));
    let mut track: BTreeMap<T, [f64; 3]> = BTreeMap::from([(T(t0), d0)]);
    let total = (b - a).max(1e-6);
    for direction in [1i64, -1] {
        let (mut d, mut sx, mut sy, mut t) = (d0, ax, ay, t0);
        while if direction > 0 { t < b - 0.1 } else { t > a + 0.1 } {
            let (c0, c1) = if direction > 0 { (t, b.min(t + FOLLOW_CHUNK_S)) } else { (a.max(t - FOLLOW_CHUNK_S), t) };
            let m = privacy::local_view(&d);
            let (frames, times, fps) = render_local(job, &sess.session, &sess.result, c0, c1, &m, fov, size)?;
            if frames.len() < 2 {
                break;
            }
            let k0 = nearest(&times, t);
            let box0 = privacy::tight_box(&d, sx, sy, &m, fov, size).context("objet hors de la vue locale")?;
            let boxes = with_tracker(|net| privacy::follow(net, &frames, k0, box0, direction, fps))?;
            let to_sphere = |bx: &[i32; 4]| privacy::box_to_sphere(&bx.map(|v| v as f64), &m, fov, size as f64, size as f64);
            for (k, bx) in &boxes {
                track.insert(T(times[*k]), to_sphere(bx).0);
            }
            let kend = if direction > 0 { *boxes.keys().max().unwrap() } else { *boxes.keys().min().unwrap() };
            let reached_end = kend == if direction > 0 { frames.len() - 1 } else { 0 };
            (d, sx, sy) = to_sphere(&boxes[&kend]);
            t = times[kend];
            job.set("progress", (track.len() as f64 * privacy::MANUAL_STEP as f64 / fps / total).min(1.0));
            let (lo, hi) = (track.keys().next().unwrap().0, track.keys().next_back().unwrap().0);
            job.set("message", format!("suivi : {lo:.0}–{hi:.0} s"));
            if !reached_end {
                break; // objet perdu : on s'arrête dans ce sens
            }
        }
    }
    Ok(track.into_iter().map(|(t, d)| (t.0, d)).collect())
}
