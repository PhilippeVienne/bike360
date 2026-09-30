//! Appels aux modules pas encore portés en Rust (telemetry, endcard, finishing, basemap,
//! privacy, et le suivi d'un compagnon qui repose sur privacy).
//!
//! Chaque fonction garde une signature proche de la fonction Python qu'elle appelle. Deux
//! sortes d'implémentations provisoires :
//! - **pont Python** : sous-processus `python3 -c <pending_bridge.py> <fonction>`, avec
//!   PYTHONPATH = dossier du code Python, arguments JSON sur l'entrée standard, résultat JSON
//!   (et progression de la tâche) sur la sortie standard ;
//! - **copie directe** : quelques fonctions triviales (lecture/écriture de fichiers JSON,
//!   validation d'un style, petite géométrie) recopiées ici pour ne pas lancer Python à chaque
//!   requête. Elles devront être remplacées par celles des modules Rust correspondants.
//!
//! Quand un module sera porté, remplacer le corps de la fonction par l'appel Rust natif
//! (le reste du serveur n'appelle que ces fonctions).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use insta_core::geometry::{self, Mat3};
use insta_core::paths;
use serde_json::{json, Map, Value};

use crate::app::{render_bin, App, Job};
use crate::pyjson;

const BRIDGE: &str = include_str!("pending_bridge.py");

/// Encodeur NVENC utilisable (testé au démarrage par le serveur), transmis au pont Python.
pub static NVENC: AtomicBool = AtomicBool::new(false);

/// Dossier du code Python : $INSTA_PYTHON_DIR, sinon la racine si elle contient server.py,
/// sinon le dépôt compilé.
pub fn python_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("INSTA_PYTHON_DIR") {
        return PathBuf::from(p);
    }
    let root = paths::root();
    if root.join("server.py").exists() {
        return root;
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}

/// Lance une fonction du pont Python sans suivi de tâche.
fn call(func: &str, args: &Value) -> Result<Value> {
    call_streaming(func, args, None, |_| {})
}

/// Lance une fonction du pont Python. `job` : tâche qui reçoit les processus lancés
/// (annulation) ; `on_job` reçoit chaque mise à jour d'état (« @job ») en direct.
fn call_streaming(func: &str, args: &Value, job: Option<&Job>, mut on_job: impl FnMut(&Map<String, Value>))
                  -> Result<Value> {
    let root = std::fs::canonicalize(paths::root()).unwrap_or_else(|_| paths::root());
    let mut child = Command::new("python3")
        .arg("-c").arg(BRIDGE).arg(func)
        .env("PYTHONPATH", python_dir())
        .env("INSTA_BUILD_ROOT", &root)
        .env("INSTA_RENDER_BIN", render_bin())
        .env("INSTA_NVENC", if NVENC.load(Ordering::SeqCst) { "1" } else { "0" })
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("lancement de python3")?;
    let pid = child.id();
    if let Some(j) = job {
        j.add_pid(pid);
    }
    let input = serde_json::to_vec(args)?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let stderr = child.stderr.take().unwrap();
    let err_buf = Arc::new(Mutex::new(String::new()));
    let err_thread = {
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("  [python] {line}");
                let mut b = buf.lock().unwrap();
                b.push_str(&line);
                b.push('\n');
                if b.len() > 8000 {
                    let cut = b.len() - 4000;
                    let cut = (cut..b.len()).find(|i| b.is_char_boundary(*i)).unwrap_or(0);
                    b.drain(..cut);
                }
            }
        })
    };
    let mut result = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        if let Some(r) = line.strip_prefix("@result ") {
            result = Some(serde_json::from_str::<Value>(r)?);
        } else if let Some(u) = line.strip_prefix("@job ") {
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(u) {
                on_job(&m);
            }
        } else if let Some(p) = line.strip_prefix("@pid ") {
            if let (Some(j), Ok(p)) = (job, p.trim().parse()) {
                j.add_pid(p);
            }
        }
    }
    let status = child.wait()?;
    let _ = writer.join();
    let _ = err_thread.join();
    if let Some(j) = job {
        j.remove_pid(pid);
    }
    if !status.success() {
        if job.is_some_and(|j| j.is_cancelled()) {
            bail!("annulé");
        }
        let err = err_buf.lock().unwrap().clone();
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("erreur Python").to_string();
        bail!("{}", last.split_once(": ").map(|(_, m)| m.to_string()).unwrap_or(last));
    }
    result.ok_or_else(|| anyhow!("pas de résultat du pont Python ({func})"))
}

/// Mises à jour d'état transmises telles quelles à la tâche (sauf clés internes).
fn forward(job: &Job) -> impl FnMut(&Map<String, Value>) + '_ {
    move |m| {
        for (k, v) in m {
            if k != "blur_progress" {
                job.set(k, v.clone());
            }
        }
    }
}

fn mat_flat(m: &Mat3) -> Vec<f64> {
    m.iter().flatten().copied().collect()
}

// ================================================================ telemetry

/// Copie directe de `telemetry.DEFAULTS` → à remplacer par la constante du module Rust telemetry.
pub fn telemetry_defaults() -> Map<String, Value> {
    let Value::Object(m) = json!({"enabled": false, "speed": true, "map": true, "altitude": true, "place": true}) else {
        unreachable!()
    };
    m
}

/// Mini-carte d'aperçu (PNG `size`², session courante en couleur, point au début du clip).
/// Remplacera : `telemetry._map_panel` + `_dot`/`_over` (dessin de `server.minimap`) — pont Python.
pub fn minimap(result: &Value, clip: &Value, tracks: &[Value], size: u32, out: &Path) -> Result<()> {
    call("minimap", &json!({"result": result, "clip": clip, "tracks": tracks, "size": size, "out": out}))?;
    Ok(())
}

/// Télémétrie pour le moteur GPU : (images PNG, placements par image) ou None sans GPS.
/// `time_map` : (temps de sortie, temps de session) interpolés (hyperlapse).
/// Remplacera : `telemetry.layers` — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn telemetry_layers(result: &Value, clip: &Value, t0: f64, n_frames: usize, fps: f64, w: u32, h: u32, opts: &Value,
                        first_part: bool, workdir: &Path, time_map: Option<(&[f64], &[f64])>, tracks: Option<&[Value]>)
                        -> Result<Option<(Value, Value)>> {
    let r = call("telemetry_layers", &json!({
        "result": result, "clip": clip, "t0": t0, "n_frames": n_frames, "fps": fps, "W": w, "H": h, "opts": opts,
        "first_part": first_part, "workdir": workdir, "time_map": time_map.map(|(a, b)| json!([a, b])),
        "tracks": tracks}))?;
    Ok(match r {
        Value::Array(mut a) if a.len() == 2 => {
            let overlays = a.pop().unwrap();
            Some((a.pop().unwrap(), overlays))
        }
        _ => None,
    })
}

/// Incruste la télémétrie sur un morceau (export ffmpeg) → `out` ; faux sans GPS.
/// Remplacera : `telemetry.overlay` — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn telemetry_overlay(part: &Path, out: &Path, result: &Value, clip: &Value, t0: f64, dur: f64, w: u32, h: u32,
                         opts: &Value, first_part: bool, encoder_args: &[String], workdir: &Path, tracks: &[Value])
                         -> Result<bool> {
    let r = call("telemetry_overlay", &json!({
        "part": part, "out": out, "result": result, "clip": clip, "t0": t0, "dur": dur, "W": w, "H": h,
        "opts": opts, "first_part": first_part, "encoder_args": encoder_args, "workdir": workdir,
        "tracks": tracks}))?;
    Ok(r.as_bool().unwrap_or(false))
}

// ================================================================ endcard

/// Carte de fin (PNG w×h) : carte des trajets, chiffres, crédits de la musique.
/// Remplacera : `endcard.render` — pont Python.
pub fn endcard_render(results: &[Value], w: u32, h: u32, path: &Path, title: &str, credits: &[String]) -> Result<()> {
    call("endcard_render", &json!({"results": results, "W": w, "H": h, "path": path, "title": title,
                                   "credits": credits}))?;
    Ok(())
}

// ================================================================ finishing

const TRANSITIONS: [(&str, Option<&str>); 4] =
    [("aucune", None), ("fondu", Some("fade")), ("noir", Some("fadeblack")), ("glisse", Some("slideleft"))];

fn finishing_defaults() -> Map<String, Value> {
    let Value::Object(m) = json!({"transition": "fondu", "duration": 0.6, "title": "", "subtitle": "",
                                  "music": "", "music_volume": 0.8, "original_volume": 0.35, "end_card": true}) else {
        unreachable!()
    };
    m
}

fn py_float(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        Value::Bool(b) => *b as i32 as f64,
        _ => 0.0,
    }
}

fn py_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Style validé (valeurs bornées, clés connues).
/// Copie directe de `finishing.clean` → à remplacer par le module Rust finishing.
pub fn finishing_clean(style: Option<&Value>) -> Value {
    let mut s = finishing_defaults();
    if let Some(Value::Object(m)) = style {
        for (k, v) in m {
            if s.contains_key(k) {
                s.insert(k.clone(), v.clone());
            }
        }
    }
    if !s["transition"].as_str().is_some_and(|t| TRANSITIONS.iter().any(|(k, _)| *k == t)) {
        s.insert("transition".into(), "fondu".into());
    }
    let clamp = |v: &Value, lo: f64, hi: f64| Value::from(py_float(v).clamp(lo, hi));
    let d = clamp(&s["duration"], 0.2, 2.0);
    s.insert("duration".into(), d);
    for k in ["music_volume", "original_volume"] {
        let v = clamp(&s[k], 0.0, 1.5);
        s.insert(k.into(), v);
    }
    for k in ["title", "subtitle", "music"] {
        let t: String = pyjson::py_str(&s[k]).chars().take(120).collect();
        s.insert(k.into(), t.into());
    }
    let e = py_truthy(&s["end_card"]);
    s.insert("end_card".into(), e.into());
    Value::Object(s)
}

/// Rien à faire au-delà d'un simple assemblage (copie sans réencodage).
/// Copie directe de `finishing.is_plain` → à remplacer par le module Rust finishing.
pub fn finishing_is_plain(style: &Value) -> bool {
    let t = style["transition"].as_str().unwrap_or("fondu");
    TRANSITIONS.iter().find(|(k, _)| *k == t).and_then(|(_, v)| *v).is_none()
        && style["title"].as_str().unwrap_or("").is_empty()
        && style["music"].as_str().unwrap_or("").is_empty()
        && !style["end_card"].as_bool().unwrap_or(false)
}

/// Assemble les clips avec transitions, titre, musique et carte de fin → `final`.
/// Remplacera : `finishing.finish` — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn finishing_finish(clip_files: &[PathBuf], final_: &Path, style: &Value, encoder_args: &[String], w: u32, h: u32,
                        music: Option<&Path>, audio_bitrate: &str, end_card: Option<&Path>, credits: &[String],
                        job: &Job) -> Result<()> {
    call_streaming("finishing_finish", &json!({
        "clip_files": clip_files, "final": final_, "style": style, "encoder_args": encoder_args, "W": w, "H": h,
        "music": music, "audio_bitrate": audio_bitrate, "end_card": end_card, "credits": credits}), Some(job), |_| {})?;
    Ok(())
}

// ================================================================ privacy

pub fn privacy_data_dir() -> PathBuf {
    paths::data().join("privacy")
}

/// Vignettes des zones détectées.
pub fn privacy_thumbs_dir() -> PathBuf {
    paths::cache().join("privacy")
}

/// Zones d'une session {clip: {key, tracks, manual}}.
/// Copie directe de `privacy.load` → à remplacer par le module Rust privacy.
pub fn privacy_load(sid: &str) -> Map<String, Value> {
    std::fs::read_to_string(privacy_data_dir().join(format!("{sid}.json")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Copie directe de `privacy.save` (même format : `json.dumps` compact).
pub fn privacy_save(sid: &str, data: &Map<String, Value>) -> Result<()> {
    std::fs::create_dir_all(privacy_data_dir())?;
    std::fs::write(privacy_data_dir().join(format!("{sid}.json")), pyjson::dumps(&Value::Object(data.clone())))?;
    Ok(())
}

/// Pistes détectées + zones tracées à la main d'un clip.
/// Copie directe de `privacy.all_tracks`.
pub fn privacy_all_tracks(entry: Option<&Value>) -> Vec<Value> {
    let list = |k: &str| entry.and_then(|e| e.get(k)).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = list("tracks");
    out.extend(list("manual"));
    out
}

/// Empreinte du cadrage/temps d'un clip (l'analyse est à refaire si elle change).
/// Copie directe de `privacy.view_key` (`json.dumps(…, sort_keys=True)` à l'identique).
pub fn privacy_view_key(clip: &Map<String, Value>) -> String {
    let mut m = Map::new();
    for k in ["start", "end", "yaw", "pitch", "roll", "fov", "horizon", "keyframes"] {
        m.insert(k.into(), clip.get(k).cloned().unwrap_or(Value::Null));
    }
    pyjson::dumps_sorted(&Value::Object(m))
}

fn ray(u: f64, v: f64, w: f64, h: f64, hfov: f64) -> [f64; 3] {
    let th = (hfov.to_radians() / 2.0).tan();
    let tv = th * h / w;
    [(2.0 * u / w - 1.0) * th, -(2.0 * v / h - 1.0) * tv, 1.0]
}

fn norm(a: [f64; 3]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}

/// Boîte écran → (direction caméra unitaire, demi-angle horizontal, vertical) en radians.
/// Copie directe de `privacy.box_to_sphere` → à remplacer par le module Rust privacy.
pub fn box_to_sphere(bx: (f64, f64, f64, f64), m: &Mat3, hfov: f64, w: f64, h: f64) -> ([f64; 3], f64, f64) {
    let (x, y, bw, bh) = bx;
    let c = ray(x + bw / 2.0, y + bh / 2.0, w, h, hfov);
    let ex = ray(x + bw, y + bh / 2.0, w, h, hfov);
    let ey = ray(x + bw / 2.0, y + bh, w, h, hfov);
    let ang = |a: [f64; 3], b: [f64; 3]| {
        let d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        (d / (norm(a) * norm(b))).clamp(-1.0, 1.0).acos()
    };
    let n = norm(c);
    let d = geometry::apply(m, c.map(|v| v / n));
    let nd = norm(d);
    (d.map(|v| v / nd), ang(c, ex), ang(c, ey))
}

/// Zones à flouter par image de sortie [[x, y, w, h]] pour le moteur GPU.
/// Remplacera : `privacy.frame_boxes` — pont Python.
pub fn privacy_frame_boxes(times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Value], w: u32, h: u32) -> Result<Value> {
    call("privacy_frame_boxes", &json!({"times": times, "mats": mats.iter().map(mat_flat).collect::<Vec<_>>(),
                                        "fovs": fovs, "tracks": tracks, "W": w, "H": h}))
}

/// Floute les zones des pistes dans `src` → `dst` ; `detect` : détection image par image en plus
/// (hyperlapse). `progress(f)` de 0 à 1.
/// Remplacera : `privacy.Detector` + `privacy.blur_video` — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn privacy_blur_video(src: &Path, dst: &Path, times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Value], w: u32,
                          h: u32, encoder_args: &[String], detect: bool, job: &Job, mut progress: impl FnMut(f64))
                          -> Result<()> {
    call_streaming("privacy_blur_video", &json!({
        "src": src, "dst": dst, "times": times, "mats": mats.iter().map(mat_flat).collect::<Vec<_>>(), "fovs": fovs,
        "tracks": tracks, "W": w, "H": h, "encoder_args": encoder_args, "detect": detect}), Some(job), |m| {
        if let Some(f) = m.get("blur_progress").and_then(Value::as_f64) {
            progress(f);
        }
    })?;
    Ok(())
}

/// Sessions (analyse, segments, horizon prêt) transmises au pont pour les fonctions de server.py.
fn sessions_payload(app: &App, sids: &[&str]) -> Result<Value> {
    let mut out = Map::new();
    for sid in sids {
        if out.contains_key(*sid) {
            continue;
        }
        let s = app.sess(sid).with_context(|| format!("session inconnue : {sid}"))?;
        out.insert(sid.to_string(), json!({"session": s.session, "result": s.result,
                                            "horizon": app.horizon_done(sid).map(|d| (*d).clone())}));
    }
    Ok(Value::Object(out))
}

/// Analyse de confidentialité (visages et plaques) de clips dans leur cadrage : rendu GPU,
/// détection, pistes enregistrées dans data/privacy/. Retourne (zones trouvées, clips à jour).
/// Remplacera : `server.analyze_privacy` (+ `privacy.Detector`, `privacy.analyze_clip`) — pont Python.
pub fn analyze_privacy(app: &App, job: &Job, items: &[(String, Map<String, Value>)], force: bool, label: &str)
                       -> Result<(u64, u64)> {
    let sids: Vec<&str> = items.iter().map(|(s, _)| s.as_str()).collect();
    let r = call_streaming("privacy_analyze", &json!({
        "sessions": sessions_payload(app, &sids)?, "items": items.iter().map(|(s, c)| json!([s, c])).collect::<Vec<_>>(),
        "force": force, "label": label}), Some(job), forward(job))?;
    Ok((r[0].as_u64().unwrap_or(0), r[1].as_u64().unwrap_or(0)))
}

/// Zone tracée à la main à l'instant t0 : suivie dans le temps (VitTrack) ou fixe sur le clip ;
/// enregistrée dans data/privacy/. L'état final (done/error) est écrit dans `job`.
/// Remplacera : `server.run_manual_zone` (+ `privacy.follow`, `local_view`, `tight_box`…) — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn privacy_manual_zone(app: &App, job: &Job, sid: &str, clip: &Map<String, Value>, t0: f64, d0: [f64; 3], ax: f64,
                           ay: f64, track_it: bool) -> Result<()> {
    call_streaming("privacy_manual_zone", &json!({
        "sessions": sessions_payload(app, &[sid])?, "sid": sid, "clip": clip, "t0": t0, "d0": d0, "ax": ax, "ay": ay,
        "track_it": track_it}), Some(job), forward(job))?;
    Ok(())
}

/// Suit un objet sur la sphère entre a et b depuis t0 (vues locales rendues par le moteur GPU,
/// suivi VitTrack) : [(t, direction)] triés.
/// Remplacera : `server.track_sphere` (+ `server.render_local`, `privacy.follow`…) — pont Python.
#[allow(clippy::too_many_arguments)]
pub fn follow_track(app: &App, job: &Job, sid: &str, t0: f64, d0: [f64; 3], ax: f64, ay: f64, a: f64, b: f64)
                    -> Result<Vec<(f64, [f64; 3])>> {
    let s = app.sess(sid).context("session inconnue")?;
    let r = call_streaming("follow_track", &json!({
        "session": s.session, "result": s.result, "t0": t0, "d0": d0, "ax": ax, "ay": ay, "a": a, "b": b}),
        Some(job), forward(job))?;
    let track: Vec<(f64, [f64; 3])> = serde_json::from_value(r)?;
    Ok(track)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_style() {
        let s = finishing_clean(Some(&json!({"duration": 5, "title": 12, "junk": 1, "end_card": 0})));
        assert_eq!(s["duration"], json!(2.0));
        assert_eq!(s["title"], json!("12"));
        assert_eq!(s["end_card"], json!(false));
        assert!(s.get("junk").is_none());
    }
}
