//! Vignettes, exports (ffmpeg ou moteur GPU), résumé hyperlapse et tâches de confidentialité
//! et de suivi.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use bike360_core::analyze::Analysis;
use bike360_core::geometry::{self, Clip, HorizonMode, Mat3};
use bike360_core::horizon::{self, HorizonData};
use bike360_core::insta360::{Segment, Session};
use bike360_core::numeric::{interp, round_nd, unwrap};
use bike360_core::lean::{self, LeanTrack};
use bike360_core::telemetry::{self, Span};
use bike360_core::{chapters, draw, endcard, finishing, hyperlapse, musiclib, ramp};
use serde_json::{json, Map, Value};

use crate::app::{exports_dir, render_bin, thumbs_dir, App, Job};
use crate::{privacy, pyjson};
use bike360_core::privacy::Track;

/// Objectifs X5 : ~195° utiles par fisheye (v360 dfisheye : yaw 0 = moitié droite du .lrv).
pub const LENS_FOV: f64 = 195.0;
pub const HEIGHTS: [u32; 4] = [720, 1080, 1440, 2160];
pub const HYPERLAPSE_MBPS_1080: f64 = 12.0; // plafond du résumé (en accéléré, le débit exploserait)
pub const FORMATS: [&str; 4] = ["standard", "vertical", "carre", "leger"];
pub const FINAL_CRF: i64 = 20;

/// Qualité d'export (QUALITY + options + FORMATS de la version Python).
#[derive(Debug, Clone)]
pub struct Quality {
    pub source: &'static str,
    pub height: u32,
    pub crf: i64,
    pub preset: &'static str,
    pub format: String,
    pub size: Option<(u32, u32)>,
    pub max_bitrate: Option<u64>,
    pub audio: String,
}

impl Quality {
    pub fn new(quality: &str, opts: &Map<String, Value>) -> Quality {
        let mut q = match quality {
            "final" => Quality { source: "insv", height: 1080, crf: 20, preset: "medium", format: "standard".into(),
                                 size: None, max_bitrate: None, audio: "160k".into() },
            _ => Quality { source: "lrv", height: 720, crf: 23, preset: "veryfast", format: "standard".into(),
                           size: None, max_bitrate: None, audio: "160k".into() },
        };
        if let Some(h) = opts.get("height").and_then(Value::as_u64) {
            q.height = h as u32;
        }
        if let Some(c) = opts.get("crf").and_then(Value::as_i64) {
            q.crf = c;
        }
        if let Some(f) = opts.get("format").and_then(Value::as_str) {
            q.format = f.into();
        }
        match q.format.as_str() {
            "vertical" => (q.size, q.max_bitrate) = (Some((1080, 1920)), Some(16_000_000)),
            "carre" => (q.size, q.max_bitrate) = (Some((1080, 1080)), Some(12_000_000)),
            "leger" => {
                (q.size, q.max_bitrate) = (Some((1280, 720)), Some(1_400_000));
                q.audio = "96k".into();
            }
            _ => {}
        }
        q
    }

    /// (largeur, hauteur) de sortie selon la destination (16:9 à la hauteur choisie par défaut).
    pub fn output_size(&self) -> (u32, u32) {
        self.size.unwrap_or((self.height * 16 / 9, self.height))
    }
}

/// Options d'export validées (qualité finale : résolution, qualité, destination).
pub fn export_opts(body: &Value, quality: &str) -> Map<String, Value> {
    let mut opts = Map::new();
    if quality == "final" {
        let h = int_of(body.get("height"), 1080);
        if HEIGHTS.contains(&(h as u32)) {
            opts.insert("height".into(), h.into());
        }
        opts.insert("crf".into(), int_of(body.get("crf"), FINAL_CRF).clamp(14, 28).into());
        let f = body.get("format").and_then(Value::as_str).filter(|f| FORMATS.contains(f)).unwrap_or("standard");
        opts.insert("format".into(), f.into());
    }
    opts
}

/// `int(x)` de Python (nombre ou texte), valeur par défaut si absent.
pub fn int_of(v: Option<&Value>, d: i64) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(d as f64) as i64),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(d),
        Some(Value::Bool(b)) => *b as i64,
        _ => d,
    }
}

/// `float(x)` de Python, valeur par défaut si absent.
pub fn float_of(v: Option<&Value>, d: f64) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(d),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(d),
        Some(Value::Bool(b)) => *b as i64 as f64,
        _ => d,
    }
}

pub fn encoder_args(app: &App, q: &Quality) -> Vec<String> {
    let rate: Vec<String> = q.max_bitrate
        .map(|c| vec!["-maxrate".into(), c.to_string(), "-bufsize".into(), c.to_string()])
        .unwrap_or_default();
    let mut a: Vec<String> = if app.nvenc_available() {
        // -cq ≈ -crf de x264 ; p5 = bon compromis qualité/vitesse
        ["-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", &q.crf.to_string(), "-b:v", "0"]
            .iter().map(|s| s.to_string()).collect()
    } else {
        ["-c:v", "libx264", "-preset", q.preset, "-crf", &q.crf.to_string()].iter().map(|s| s.to_string()).collect()
    };
    a.extend(rate);
    if app.nvenc_available() {
        a.extend(["-pix_fmt".to_string(), "yuv420p".to_string()]);
    }
    a
}

pub fn v_fov_of(h_fov: f64, w: f64, h: f64) -> f64 {
    (2.0 * ((h_fov.to_radians() / 2.0).tan() * h / w).atan()).to_degrees()
}

/// Champ horizontal de sortie pour un champ de clip défini en 16:9.
pub fn output_fov(fov: f64, w: u32, h: u32) -> f64 {
    if (w * 9) as i64 >= (h * 16) as i64 - 1 { fov } else { v_fov_of(fov, 16.0, 9.0) }
}

pub fn to_clip(c: &Map<String, Value>) -> Result<Clip> {
    serde_json::from_value(Value::Object(c.clone())).context("clip invalide")
}

fn cstart(c: &Map<String, Value>) -> f64 {
    c.get("start").and_then(Value::as_f64).unwrap_or(0.0)
}
fn cend(c: &Map<String, Value>) -> f64 {
    c.get("end").and_then(Value::as_f64).unwrap_or(0.0)
}

// ---------------------------------------------------------------- vignettes et mini-carte

fn sha16(s: &str) -> String {
    sha1_smol::Sha1::from(s).digest().to_string()[..16].to_string()
}

/// Vignette JPEG (vue plane) d'une session à l'instant t, mise en cache.
pub fn thumbnail(app: &App, sid: &str, t: f64, yaw: f64, pitch: f64, fov: f64, width: u32) -> Result<PathBuf> {
    let s = app.sess(sid).context("session inconnue")?;
    let name = sha16(&format!("{sid}|{t:.1}|{yaw:.1}|{pitch:.1}|{fov:.0}|{width}"));
    let out = thumbs_dir().join(format!("{name}.jpg"));
    if out.exists() {
        return Ok(out);
    }
    let segs = &s.result.segments;
    let mut chosen = None;
    for (k, (seg, info)) in s.session.segments.iter().zip(segs).enumerate() {
        if (info.offset <= t && t < info.offset + info.duration) || k == segs.len() - 1 {
            chosen = Some((seg, (t - info.offset).min(info.duration - 0.5).max(0.0)));
            break;
        }
    }
    let (seg, local) = chosen.context("aucun segment")?;
    let lrv = seg.lrv.as_ref().context("pas de .lrv")?;
    let height = width * 9 / 16;
    let vfov = v_fov_of(fov, width as f64, height as f64);
    std::fs::create_dir_all(thumbs_dir())?;
    let f = pyjson::float_repr;
    let vf = format!("v360=input=dfisheye:ih_fov={LENS_FOV:.0}:iv_fov={LENS_FOV:.0}:output=flat:yaw={}:pitch={}\
                      :h_fov={}:v_fov={vfov:.2}:w={width}:h={height}", f(yaw), f(pitch), f(fov));
    let tmp = out.with_extension(format!("{}.jpg", std::process::id()));
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-ss", &format!("{local:.2}"), "-i"]).arg(lrv)
        .args(["-frames:v", "1", "-vf", &vf, "-q:v", "5"]).arg(&tmp)
        .status()?;
    if !st.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("ffmpeg a échoué (vignette)");
    }
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

/// Aperçu PNG de la mini-carte d'export : tracés des sessions du montage, clip en couleur.
pub fn minimap(app: &App, sid: &str, clip_id: Option<&str>, size: u32) -> Result<PathBuf> {
    let mut sids: Vec<String> = vec![sid.to_string()];
    for (x, _) in app.montage_items() {
        if !sids.contains(&x) {
            sids.push(x);
        }
    }
    let clip = app.get_selections(sid).into_iter()
        .find(|c| clip_id.is_some_and(|id| c.get("id").and_then(Value::as_str) == Some(id)))
        .map(Value::Object)
        .unwrap_or_else(|| json!({"start": 0, "end": -1}));
    let sess: Vec<_> = sids.iter().map(|x| app.sess(x).context("session inconnue")).collect::<Result<_>>()?;
    let mut tag: Vec<String> = sids.clone();
    tag.push(sid.into());
    tag.push(clip_id.unwrap_or("None").into());
    tag.push(pyjson::py_str(&clip["start"]));
    tag.push(pyjson::py_str(&clip["end"]));
    tag.push(size.to_string());
    for s in &sess {
        tag.push(pyjson::float_repr(s.result.utc_t0 + s.result.offset_s));
    }
    let out = thumbs_dir().join(format!("map_{}.png", sha16(&tag.join("|"))));
    if out.exists() {
        return Ok(out);
    }
    let result = &sess[0].result;
    let tracks: Vec<&Analysis> = sess.iter().map(|s| &s.result).collect();
    let span = Span { start: float_of(clip.get("start"), 0.0), end: float_of(clip.get("end"), -1.0) };
    let s = size as usize;
    let (mut panel, project, _) = telemetry::map_panel(result, span, &tracks, s, size as f64 / 0.26);
    if span.end > span.start {
        if let (Some(lat), Some(lon)) = (telemetry::series(result, "lat"), telemetry::series(result, "lon")) {
            let d = (8.max((size as f64 * 0.06) as usize)) / 2 * 2;
            let k = (span.start as usize).min(lat.len() - 1);
            let (x, y) = project.project(lat[k], lon[k]);
            let (x0, y0) = ((x - d as f64 / 2.0).round_ties_even() as i64, (y - d as f64 / 2.0).round_ties_even() as i64);
            if 0 <= x0 && x0 <= (s - d) as i64 && 0 <= y0 && y0 <= (s - d) as i64 {
                panel.over(&draw::dot(d), x0, y0);
            }
        }
    }
    std::fs::create_dir_all(thumbs_dir())?;
    panel.save_png(&out)?;
    Ok(out)
}

// ---------------------------------------------------------------- outils d'export

/// Taille de l'image double fisheye (les deux flux .insv sont juxtaposés).
fn source_size(path: &Path, source: &str) -> Result<(u32, u32)> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path).output()?;
    let s = String::from_utf8_lossy(&o.stdout);
    let v: Vec<u32> = s.trim().split(',').take(2).filter_map(|x| x.trim().parse().ok()).collect();
    if v.len() < 2 {
        bail!("taille de {path:?} illisible");
    }
    Ok(if source == "insv" { (v[0] * 2, v[1]) } else { (v[0], v[1]) })
}

/// Cadence d'une vidéo : fraction réduite (texte comme `str(Fraction)`) et valeur.
pub(crate) fn source_fps(path: &Path) -> Result<(String, f64)> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0"])
        .arg(path).output()?;
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    let (n, d) = match s.split_once('/') {
        Some((n, d)) => (n.trim().parse::<u64>()?, d.trim().parse::<u64>()?),
        None => (s.parse::<u64>().with_context(|| format!("cadence illisible : {s:?}"))?, 1),
    };
    if d == 0 {
        bail!("cadence illisible : {s:?}");
    }
    let g = gcd(n, d);
    let (n, d) = (n / g, d / g);
    Ok((if d == 1 { n.to_string() } else { format!("{n}/{d}") }, n as f64 / d as f64))
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a.max(1) } else { gcd(b, a % b) }
}

/// Floute les zones masquées (coordonnées 0..1 de l'image double fisheye).
fn mask_filters(masks: &[[f64; 4]], size: (u32, u32), label: &str) -> Vec<String> {
    let (bw, bh) = (size.0 as i64, size.1 as i64);
    masks.iter().enumerate().map(|(k, m)| {
        let (x, y) = ((m[0] * bw as f64) as i64 / 2 * 2, (m[1] * bh as f64) as i64 / 2 * 2);
        let (w, h) = (8.max((m[2] * bw as f64) as i64 / 2 * 2), 8.max((m[3] * bh as f64) as i64 / 2 * 2));
        let (w, h) = (w.min(bw - x), h.min(bh - y));
        let r = 2.max(w.min(h) / 5);
        format!("[{label}{k}]split[b{k}][c{k}];[c{k}]crop={w}:{h}:{x}:{y},boxblur={r}:3[m{k}];[b{k}][m{k}]overlay={x}:{y}[{label}{}]", k + 1)
    }).collect()
}

/// Redressement d'un clip à l'instant t selon son mode (auto → image, sinon support).
pub fn level_matrix_at(clip: &Clip, result: &Analysis, horizon_data: Option<&HorizonData>, t: f64) -> Option<Mat3> {
    match geometry::clip_horizon_mode(clip) {
        HorizonMode::Aucun => None,
        HorizonMode::Auto if horizon_data.is_some_and(|h| !h.up.is_empty()) => horizon::level_at(horizon_data, t),
        _ => Some(geometry::tilt_matrix(Some(&result.tilt))),
    }
}

/// (t relatif au morceau, rotation écran → caméra, champ horizontal °) pour chaque image.
pub(crate) fn part_targets(clip: &Clip, result: &Analysis, horizon_data: Option<&HorizonData>, seg_offset: f64, ss: f64, dur: f64,
                fd: f64) -> Vec<(f64, Mat3, f64)> {
    let n = (dur / fd).ceil() as usize + 1;
    (0..n).map(|i| {
        let t = i as f64 * fd;
        let t_session = seg_offset + ss + t;
        let level = level_matrix_at(clip, result, horizon_data, t_session);
        let v = geometry::clip_view_at(clip, t_session - clip.start);
        (t, geometry::view_matrix(v.yaw, v.pitch, level.as_ref(), v.roll), v.fov)
    }).collect()
}

/// Écrit un fichier sendcmd suivant des cadrages cibles (rotations différentielles, v360 étant
/// cumulatif) ; retourne les angles initiaux.
fn level_commands(targets: &[(f64, Mat3, f64)], path: &Path, w: u32, h: u32) -> Result<(f64, f64, f64)> {
    let mut lines = vec![];
    for k in 1..targets.len() {
        let (t, target, fov) = &targets[k];
        let mut cmds = vec![];
        let (dy, dp, dr) = geometry::v360_angles(&geometry::mul(&geometry::transpose(&targets[k - 1].1), target));
        if dy.abs().max(dp.abs()).max(dr.abs()) > 0.01 {
            cmds.push(format!("v360@lv yaw {dy:.4}, v360@lv pitch {dp:.4}, v360@lv roll {dr:.4}"));
        }
        if (fov - targets[k - 1].2).abs() > 0.01 {
            cmds.push(format!("v360@lv h_fov {fov:.3}, v360@lv v_fov {:.3}", v_fov_of(*fov, w as f64, h as f64)));
        }
        if !cmds.is_empty() {
            lines.push(format!("{t:.3} {};", cmds.join(", ")));
        }
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Ok(geometry::v360_angles(&targets[0].1))
}

#[allow(clippy::too_many_arguments)]
fn v360_filter(fov: f64, w: u32, h: u32, source: &str, masks: &[[f64; 4]], size: (u32, u32), angles: (f64, f64, f64),
               cmdfile: Option<&Path>) -> String {
    let v_fov = v_fov_of(fov, w as f64, h as f64);
    let (yaw, pitch, roll) = angles;
    let mut v360 = format!("v360@lv=input=dfisheye:ih_fov={LENS_FOV:.0}:iv_fov={LENS_FOV:.0}:output=flat\
                            :yaw={yaw:.3}:pitch={pitch:.3}:roll={roll:.3}:h_fov={fov:.2}:v_fov={v_fov:.2}:w={w}:h={h},setsar=1,format=yuv420p");
    if let Some(c) = cmdfile {
        v360 = format!("sendcmd=f='{}',{v360}", c.display());
    }
    // .insv : flux 0 = objectif avant, flux 1 = arrière → on reproduit la disposition du .lrv.
    let inp = if source == "insv" { "[0:v:1][0:v:0]hstack" } else { "[0:v:0]null" };
    let mut chain = vec![format!("{inp}[s0]")];
    chain.extend(mask_filters(masks, size, "s"));
    chain.push(format!("[s{}]{v360}[v]", masks.len()));
    chain.join(";")
}

/// Découpe un clip (temps de session) en morceaux par segment de fichier : (segment, début, durée).
pub fn clip_parts<'a>(session: &'a Session, result: &Analysis, start: f64, end: f64) -> Vec<(&'a Segment, f64, f64)> {
    session.segments.iter().zip(&result.segments).filter_map(|(seg, info)| {
        let s = start.max(info.offset);
        let e = end.min(info.offset + info.duration);
        (e - s > 0.05).then_some((seg, s - info.offset, e - s))
    }).collect()
}

pub(crate) fn seg_offset(result: &Analysis, seg: &Segment) -> Result<f64> {
    result.segments.iter().find(|x| x.index == seg.index).map(|x| x.offset).context("segment inconnu")
}

/// Lance un processus d'export et suit sa progression (lignes de sa sortie standard).
pub(crate) fn run_part_process(job: &Job, cmd: &[String], mut on_line: impl FnMut(&str)) -> Result<()> {
    let mut child = Command::new(&cmd[0]).args(&cmd[1..]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
        .with_context(|| format!("lancement de {}", cmd[0]))?;
    let pid = child.id();
    job.add_pid(pid);
    let mut stderr = child.stderr.take().unwrap();
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        on_line(&line);
    }
    let status = child.wait()?;
    job.remove_pid(pid);
    let err = err.join().unwrap_or_default();
    if !status.success() {
        if job.is_cancelled() {
            bail!("annulé");
        }
        let cut = err.len().saturating_sub(2000);
        let cut = (cut..=err.len()).find(|i| err.is_char_boundary(*i)).unwrap_or(0);
        bail!("{}", &err[cut..]);
    }
    Ok(())
}

fn run_checked(args: &[&str]) -> Result<()> {
    let st = Command::new(args[0]).args(&args[1..]).status()?;
    if !st.success() {
        bail!("Command '{:?}' returned non-zero exit status {}.", args, st.code().unwrap_or(-1));
    }
    Ok(())
}

fn s<T: ToString>(x: T) -> String {
    x.to_string()
}

type Views = (Vec<f64>, Vec<Mat3>, Vec<f64>);

#[allow(clippy::too_many_arguments)]
fn export_part_ffmpeg(app: &App, job: &Job, clip: &Clip, result: &Analysis, horizon_data: Option<&HorizonData>, seg: &Segment,
                      ss: f64, dur: f64, src: &Path, q: &Quality, masks: &[[f64; 4]], size: (u32, u32), out_w: u32,
                      out_h: u32, out: &Path, report: &dyn Fn(f64)) -> Result<Views> {
    let off = seg_offset(result, seg)?;
    let (_, fps) = source_fps(src)?;
    let targets: Vec<(f64, Mat3, f64)> = part_targets(clip, result, horizon_data, off, ss, dur, 1.0 / fps)
        .into_iter().map(|(t, m, f)| (t, m, output_fov(f, out_w, out_h))).collect();
    let cmdfile = out.with_extension("cmd");
    let angles = level_commands(&targets, &cmdfile, out_w, out_h)?;
    // cadrage immobile : aucune commande, et sendcmd refuse un fichier vide
    let has_cmds = !std::fs::read_to_string(&cmdfile)?.trim().is_empty();
    let graph = v360_filter(targets[0].2, out_w, out_h, q.source, masks, size, angles,
                            has_cmds.then_some(cmdfile.as_path()));
    let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y", "-ss", &format!("{ss:.3}"), "-t", &format!("{dur:.3}"), "-i"]
        .iter().map(|x| x.to_string()).collect();
    cmd.push(src.display().to_string());
    cmd.extend(["-filter_complex", &graph, "-map", "[v]", "-map", "0:a:0?"].map(s));
    cmd.extend(encoder_args(app, q));
    cmd.extend(["-c:a", "aac", "-b:a", &q.audio, "-ar", "48000", "-ac", "2", "-movflags", "+faststart",
                "-progress", "pipe:1", "-nostats"].map(s));
    cmd.push(out.display().to_string());
    run_part_process(job, &cmd, |line| {
        if let Some(v) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = v.trim().parse::<i64>() {
                report(us as f64 / 1e6);
            }
        }
    })?;
    Ok((targets.iter().map(|(t, _, _)| off + ss + t).collect(), targets.iter().map(|x| x.1).collect(),
        targets.iter().map(|x| x.2).collect()))
}

/// `effects(temps de session, matrices, champs, cadence, accéléré)` → champs du travail du moteur
/// (zones floutées, incrustations) ; `accéléré` : temps non linéaires (points de vitesse).
type Effects<'a> = &'a dyn Fn(&[f64], &[Mat3], &[f64], f64, bool) -> Result<Map<String, Value>>;

pub(crate) fn flat(m: &Mat3) -> Vec<f64> {
    m.iter().flatten().copied().collect()
}

/// Rendu NVDEC → CUDA → NVENC (render/), puis multiplexage du son par ffmpeg.
/// Clip accéléré (points de vitesse) : voir [`export_part_gpu_ramp`]. None : aucune image de
/// sortie dans ce morceau (morceau très court à grande vitesse).
#[allow(clippy::too_many_arguments)]
fn export_part_gpu(job: &Job, clip: &Clip, result: &Analysis, horizon_data: Option<&HorizonData>, seg: &Segment, ss: f64,
                   dur: f64, src: &Path, q: &Quality, masks: &[[f64; 4]], out_w: u32, out_h: u32, out: &Path,
                   report: &dyn Fn(f64), effects: Option<Effects>) -> Result<Option<Views>> {
    let keys = ramp::speed_keys(clip);
    if !keys.is_empty() {
        return export_part_gpu_ramp(job, clip, &keys, result, horizon_data, seg, ss, dur, src, q, masks, out_w, out_h,
                                    out, report, effects);
    }
    let (fps_s, fps) = source_fps(src)?;
    let fd = 1.0 / fps;
    let off = seg_offset(result, seg)?;
    let targets = part_targets(clip, result, horizon_data, off, ss, dur, fd);
    let mats: Vec<Mat3> = targets.iter().map(|x| x.1).collect();
    let fovs: Vec<f64> = targets.iter().map(|x| output_fov(x.2, out_w, out_h)).collect();
    let h264 = out.with_extension("h264");
    let spec = out.with_extension("json");
    let times: Vec<f64> = targets.iter().map(|(t, _, _)| off + ss + t).collect();
    let extra = match effects {
        Some(e) => e(&times, &mats, &fovs, fps, false)?,
        None => Map::new(),
    };
    let mut job_spec = json!({
        "source": src, "start": ss, "duration": dur, "width": out_w, "height": out_h,
        "fov": fovs[0], "fovs": fovs, "cq": q.crf, "max_bitrate": q.max_bitrate.unwrap_or(0),
        "masks": masks, "matrices": mats.iter().map(flat).collect::<Vec<_>>(), "output": h264,
    });
    for (k, v) in extra {
        job_spec[k] = v;
    }
    std::fs::write(&spec, serde_json::to_string(&job_spec)?)?;
    run_part_process(job, &[render_bin().display().to_string(), spec.display().to_string()], |line| {
        if let Some(n) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<u64>().ok()) {
            report(n as f64 * fd);
        }
    })?;
    let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y", "-framerate", &fps_s, "-i"].map(s).to_vec();
    cmd.push(h264.display().to_string());
    cmd.extend(["-ss", &format!("{ss:.3}"), "-t", &format!("{dur:.3}"), "-i"].map(s));
    cmd.push(src.display().to_string());
    cmd.extend(["-map", "0:v", "-map", "1:a:0?", "-c:v", "copy", "-c:a", "aac", "-b:a", &q.audio, "-ar", "48000",
                "-ac", "2", "-movflags", "+faststart"].map(s));
    cmd.push(out.display().to_string());
    run_part_process(job, &cmd, |_| {})?;
    let _ = std::fs::remove_file(&h264);
    Ok(Some((times, mats, fovs)))
}

const RAMP_AUDIO_MIN_S: f64 = 0.5; // passage à vitesse normale plus court : pas de son
const RAMP_AUDIO_FADE_S: f64 = 0.2;

/// Morceau d'un clip accéléré : le moteur ne rend que les images source retenues
/// (`samples`, comme l'hyperlapse, tirées de ramp::source_times) ; le son d'origine n'est gardé
/// que sur les passages à vitesse normale (ramp::audio_spans, fondus de 0,2 s), silence ailleurs.
#[allow(clippy::too_many_arguments)]
fn export_part_gpu_ramp(job: &Job, clip: &Clip, keys: &[ramp::SpeedKey], result: &Analysis,
                        horizon_data: Option<&HorizonData>, seg: &Segment, ss: f64, dur: f64, src: &Path, q: &Quality,
                        masks: &[[f64; 4]], out_w: u32, out_h: u32, out: &Path, report: &dyn Fn(f64),
                        effects: Option<Effects>) -> Result<Option<Views>> {
    let (fps_s, fps) = source_fps(src)?;
    let off = seg_offset(result, seg)?;
    let length = clip.end - clip.start;
    let all = ramp::source_times(keys, length, fps);
    // portion du clip couverte par ce morceau (s depuis le début du clip)
    let (p0, p1) = (off + ss - clip.start, off + ss + dur - clip.start);
    let (mut samples, mut times, mut first_out): (Vec<i64>, Vec<f64>, Option<usize>) = (vec![], vec![], None);
    for (k, t) in all.iter().enumerate() {
        if *t < p0 - 1e-9 || *t >= p1 - 1e-9 {
            continue;
        }
        let t_session = clip.start + t;
        let sample = ((t_session - off) * fps).round_ties_even().max(0.0) as i64;
        if samples.last() == Some(&sample) {
            continue;
        }
        first_out.get_or_insert(k);
        samples.push(sample);
        times.push(t_session);
    }
    let Some(first_out) = first_out else { return Ok(None) };
    let mut mats = vec![];
    let mut fovs = vec![];
    for t in &times {
        let level = level_matrix_at(clip, result, horizon_data, *t);
        let v = geometry::clip_view_at(clip, t - clip.start);
        mats.push(geometry::view_matrix(v.yaw, v.pitch, level.as_ref(), v.roll));
        fovs.push(output_fov(v.fov, out_w, out_h));
    }
    let extra = match effects {
        Some(e) => e(&times, &mats, &fovs, fps, true)?,
        None => Map::new(),
    };
    let h264 = out.with_extension("h264");
    let spec = out.with_extension("json");
    let mut job_spec = json!({
        "source": src, "start": 0.0, "duration": 0.0, "width": out_w, "height": out_h,
        "fov": fovs[0], "fovs": fovs, "cq": q.crf, "samples": samples, "max_bitrate": q.max_bitrate.unwrap_or(0),
        "masks": masks, "matrices": mats.iter().map(flat).collect::<Vec<_>>(), "output": h264,
    });
    for (k, v) in extra {
        job_spec[k] = v;
    }
    std::fs::write(&spec, serde_json::to_string(&job_spec)?)?;
    let n = samples.len() as f64;
    run_part_process(job, &[render_bin().display().to_string(), spec.display().to_string()], |line| {
        if let Some(k) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<u64>().ok()) {
            report(k as f64 / n * dur);
        }
    })?;

    // son : passages à vitesse normale de ce morceau → (décalage en sortie, début dans le fichier, durée)
    let o0 = first_out as f64 / fps;
    let out_dur = n / fps;
    let mut pieces: Vec<(f64, f64, f64)> = vec![];
    for sp in ramp::audio_spans(keys, length, fps, RAMP_AUDIO_MIN_S) {
        let src_end = sp.src_start + (sp.out_end - sp.out_start);
        let (a, b) = (sp.src_start.max(p0), src_end.min(p1));
        let delay = sp.out_start + (a - sp.src_start) - o0;
        let len = (b - a).min(out_dur - delay);
        if len >= 0.05 && delay > -1e-6 {
            pieces.push((delay.max(0.0), a + clip.start - off, len));
        }
    }
    let has_audio = finishing::probe(src).map(|(_, a)| a).unwrap_or(false);
    if !has_audio {
        pieces.clear();
    }
    let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y", "-framerate", &fps_s, "-i"].map(s).to_vec();
    cmd.push(h264.display().to_string());
    let mut graph = format!("anullsrc=r=48000:cl=stereo,atrim=duration={out_dur:.3}[s0]");
    if pieces.is_empty() {
        graph += ";[s0]anull[a]";
    } else {
        let a0 = pieces.iter().map(|p| p.1).fold(f64::INFINITY, f64::min).floor().max(0.0);
        let a1 = pieces.iter().map(|p| p.1 + p.2).fold(0.0, f64::max);
        cmd.extend(["-ss", &format!("{a0:.3}"), "-t", &format!("{:.3}", a1 - a0 + 1.0), "-i"].map(s));
        cmd.push(src.display().to_string());
        let labels: String = (0..pieces.len()).map(|i| format!("[x{i}]")).collect();
        graph += &format!(";[1:a:0]asplit={}{labels}", pieces.len());
        for (i, (delay, start, len)) in pieces.iter().enumerate() {
            let f = RAMP_AUDIO_FADE_S.min(len / 2.0);
            graph += &format!(";[x{i}]atrim=start={:.3}:duration={len:.3},asetpts=PTS-STARTPTS,aresample=48000,\
                               aformat=sample_fmts=fltp:channel_layouts=stereo,afade=t=in:d={f:.3},\
                               afade=t=out:st={:.3}:d={f:.3},adelay={}:all=1[a{i}]",
                              start - a0, len - f, (delay * 1000.0).round() as i64);
        }
        let inputs: String = (0..pieces.len()).map(|i| format!("[a{i}]")).collect();
        graph += &format!(";[s0]{inputs}amix=inputs={}:duration=first:normalize=0[a]", pieces.len() + 1);
    }
    cmd.extend(["-filter_complex", &graph, "-map", "0:v", "-map", "[a]", "-c:v", "copy", "-c:a", "aac", "-b:a", &q.audio,
                "-ar", "48000", "-ac", "2", "-movflags", "+faststart"].map(s));
    cmd.push(out.display().to_string());
    run_part_process(job, &cmd, |_| {})?;
    let _ = std::fs::remove_file(&h264);
    Ok(Some((times, mats, fovs)))
}

fn settings_masks(settings: &Value) -> Vec<[f64; 4]> {
    settings["masks"].as_array().into_iter().flatten()
        .map(|m| ["x", "y", "w", "h"].map(|k| m[k].as_f64().unwrap_or(0.0)))
        .collect()
}


/// Exporte une liste de clips [(session, clip)] dans l'ordre donné (une session ou un montage).
pub fn run_export(app: Arc<App>, job: Arc<Job>, key: String, clips: Vec<(String, Map<String, Value>)>, quality: String,
                  opts: Map<String, Value>) {
    let r = export_inner(&app, &job, &key, &clips, &quality, &opts);
    job.finish(r);
}

fn export_inner(app: &App, job: &Job, key: &str, clips: &[(String, Map<String, Value>)], quality: &str,
                opts: &Map<String, Value>) -> Result<()> {
    let q = Quality::new(quality, opts);
    let (out_w, out_h) = q.output_size();
    let name = if q.format == "standard" { format!("{quality}_{out_h}p") } else { q.format.clone() };
    let mut sids: Vec<String> = vec![];
    for (sid, _) in clips {
        if !sids.contains(sid) {
            sids.push(sid.clone());
        }
    }
    let settings = app.get_settings();
    let masks = settings_masks(&settings);
    let tel_opts = telemetry::Options::merged(settings["telemetry"].as_object());
    let blur_on = settings["privacy"]["enabled"].as_bool().unwrap_or(false);
    if clips.is_empty() {
        bail!("aucun clip sélectionné");
    }
    let total_s: f64 = clips.iter().map(|(_, c)| cend(c) - cstart(c)).sum();
    if q.format == "vertical" && total_s > 90.0 {
        job.set("warning", format!("{} min {:02} : long pour un Reel (90 s) ou un Short (60 s)",
                                   (total_s / 60.0).floor() as i64, (total_s % 60.0) as i64));
    }
    let gpu = q.source == "insv" && app.gpu_engine_available();
    let engine = if gpu { "GPU" } else { "ffmpeg" };
    job.set("engine", engine);
    if blur_on {
        // floutage demandé : aucun clip ne doit échapper à l'analyse (ajouté ou recadré depuis)
        let todo = privacy::pending(clips);
        if !todo.is_empty() {
            privacy::analyze(app, job, &todo, false, &format!("confidentialité ({} clip(s) à analyser) · ", todo.len()))?;
        }
    }
    let out_dir = exports_dir().join(key).join(&name);
    std::fs::create_dir_all(&out_dir)?;
    let sessions: Vec<_> = clips.iter().map(|(sid, _)| app.sess(sid).context("session inconnue")).collect::<Result<_>>()?;
    let parsed: Vec<Clip> = clips.iter().map(|(_, c)| to_clip(c)).collect::<Result<_>>()?;
    let mut todo = vec![];
    for (i, s) in sessions.iter().enumerate() {
        for (seg, ss, dur) in clip_parts(&s.session, &s.result, parsed[i].start, parsed[i].end) {
            todo.push((i, seg.clone(), ss, dur));
        }
    }
    let total: f64 = todo.iter().map(|x| x.3).sum();
    let mut done = 0.0;
    let mut files: Vec<(usize, PathBuf)> = vec![];
    let mut clip_horizon: std::collections::HashMap<usize, Option<HorizonData>> = Default::default();
    let montage: Vec<Arc<crate::app::Sess>> = sids.iter().map(|x| app.sess(x).context("session inconnue")).collect::<Result<_>>()?;
    let tracks_all: Vec<&Analysis> = montage.iter().map(|s| &s.result).collect();
    let mut sizes: std::collections::HashMap<PathBuf, (u32, u32)> = Default::default();
    let n_clips = clips.len();
    for (n, (i, seg, ss, dur)) in todo.iter().enumerate() {
        let (i, ss, dur) = (*i, *ss, *dur);
        let (sid, raw) = &clips[i];
        let clip = &parsed[i];
        let sess = &sessions[i];
        let result = &sess.result;
        // Horizon : analyse complète si déjà prête, sinon seulement les portions des clips.
        let full = app.horizon_done(sid);
        let mut hdata: Option<&HorizonData> = full.as_deref();
        if geometry::clip_horizon_mode(clip) == HorizonMode::Auto && hdata.is_none() {
            if !clip_horizon.contains_key(&i) {
                job.set("message", format!("horizon du clip {}/{n_clips}", i + 1));
                clip_horizon.insert(i, horizon::compute_range(&sess.session, result, clip.start - 3.0, clip.end + 3.0));
            }
            hdata = clip_horizon[&i].as_ref();
        }
        let src = if q.source == "insv" { seg.insv.clone() } else { seg.lrv.clone() };
        let Some(src) = src else { bail!("fichier {} manquant pour le segment {} de {sid}", q.source, seg.index) };
        let out = out_dir.join(format!("part_{n:03}.mp4"));
        job.set("message", format!("clip {}/{n_clips} ({engine})", i + 1));
        let base = done;
        let report = |t: f64| job.set("progress", ((base + t) / total).min(1.0));
        let tracks: Vec<Track> = if blur_on {
            privacy::clip_tracks(sid, raw.get("id")).into_iter().filter(Track::is_enabled).collect()
        } else {
            vec![]
        };
        let tel_on = tel_opts.enabled && result.gps_coverage > 0.3;
        // jauge d'inclinaison : horizon de la session (ou de la portion du clip) s'il est calculé
        let lean_track = hdata.filter(|_| tel_on && tel_opts.lean).map(|h| LeanTrack::new(h, &result.tilt));
        if gpu {
            // floutage et télémétrie faits par le moteur pendant le rendu
            let effects = |times: &[f64], mats: &[Mat3], fovs: &[f64], fps: f64, ramped: bool| -> Result<Map<String, Value>> {
                let mut extra = Map::new();
                if !tracks.is_empty() {
                    extra.insert("blur".into(), privacy::frame_boxes(times, mats, fovs, &tracks, out_w, out_h));
                }
                if tel_on {
                    // accéléré : temps de session de chaque image de sortie (interpolé)
                    let out_t: Vec<f64> = (0..times.len()).map(|k| k as f64 / fps).collect();
                    let time_map = |t: f64| interp(t, &out_t, times);
                    let tm: Option<&dyn Fn(f64) -> f64> = if ramped { Some(&time_map) } else { None };
                    let lay = telemetry::layers(result, Span::from(clip), times[0], times.len(), fps, out_w as usize,
                                                out_h as usize, &tel_opts, (times[0] - clip.start).abs() < 0.5,
                                                &out_dir.join(format!("tel_{n:03}")), tm, &tracks_all,
                                                lean_track.as_ref())?;
                    if let Some(l) = lay {
                        extra.insert("sprites".into(), json!(l.sprites));
                        extra.insert("overlays".into(), json!(l.overlays));
                    }
                }
                Ok(extra)
            };
            let rendered = export_part_gpu(job, clip, result, hdata, seg, ss, dur, &src, &q, &masks, out_w, out_h, &out,
                                           &report, Some(&effects))?;
            done += dur;
            if rendered.is_some() {
                files.push((i, out));
            }
            continue;
        }
        if !sizes.contains_key(&src) {
            sizes.insert(src.clone(), source_size(&src, q.source)?);
        }
        let views = export_part_ffmpeg(app, job, clip, result, hdata, seg, ss, dur, &src, &q, &masks, sizes[&src], out_w,
                                       out_h, &out, &report)?;
        if blur_on {
            let tracks = privacy::clip_tracks(sid, raw.get("id"));
            if tracks.iter().any(Track::is_enabled) {
                job.set("message", format!("clip {}/{n_clips} : floutage", i + 1));
                let blurred = out.with_file_name(format!("part_{n:03}_flou.mp4"));
                privacy::blur_video(&out, &blurred, &views.0, &views.1, &views.2, &tracks, out_w, out_h,
                                    &encoder_args(app, &q), false, job, |_| {})?;
                std::fs::rename(&blurred, &out)?;
            }
        }
        if tel_on {
            job.set("message", format!("clip {}/{n_clips} : télémétrie", i + 1));
            let off = seg_offset(result, seg)?;
            let with_tel = out.with_file_name(format!("part_{n:03}_tel.mp4"));
            if telemetry::overlay(&out, &with_tel, result, Span::from(clip), off + ss, dur, out_w as usize,
                                  out_h as usize, &tel_opts, (off + ss - clip.start).abs() < 0.5, &encoder_args(app, &q),
                                  &out_dir.join(format!("tel_{n:03}")), None, &tracks_all)? {
                std::fs::rename(&with_tel, &out)?;
            }
        }
        done += dur;
        files.push((i, out));
    }
    job.set("message", "assemblage");
    let prefix = if key == "montage" {
        let min = sids.iter().min().unwrap();
        format!("montage_{}_{}clips", min.get(4..12).unwrap_or(""), clips.len())
    } else {
        key.to_string()
    };
    let final_ = exports_dir().join(format!("{prefix}_{name}.mp4"));
    let style = (key == "montage").then(|| app.get_project()["style"].clone());
    let mut chapter_text = None;
    match &style {
        Some(style) if !finishing::is_plain(style) => {
            // un fichier par clip (morceaux d'un même clip recollés sans transition), puis finition
            let mut clip_files = vec![];
            let mut seen = vec![];
            for (i, _) in &files {
                if seen.contains(i) {
                    continue;
                }
                seen.push(*i);
                let parts: Vec<&PathBuf> = files.iter().filter(|(j, _)| j == i).map(|(_, f)| f).collect();
                if parts.len() == 1 {
                    clip_files.push(parts[0].clone());
                    continue;
                }
                let joined = out_dir.join(format!("clip_{i:03}.mp4"));
                let listing = out_dir.join(format!("clip_{i:03}.txt"));
                std::fs::write(&listing, parts.iter().map(|f| format!("file '{}'\n", f.file_name().unwrap().to_string_lossy())).collect::<String>())?;
                run_checked(&["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", &listing.display().to_string(),
                              "-c", "copy", &joined.display().to_string()])?;
                clip_files.push(joined);
            }
            let audio = crate::audio::resolve(style);
            let mut credits: Vec<String> = vec![];
            for t in &audio {
                let name = t.spec["file"].as_str().unwrap_or_default();
                if t.spec["muted"] != json!(true) {
                    for l in musiclib::credit_lines(name) {
                        if !credits.contains(&l) {
                            credits.push(l);
                        }
                    }
                }
            }
            let mut card = None;
            if style["end_card"].as_bool().unwrap_or(false) {
                let c = out_dir.join("fin.png");
                // record d'angle : sessions dont l'horizon est calculé
                let lean_stats: Vec<lean::LeanStats> = sids.iter().zip(&montage)
                    .filter_map(|(sid, s)| app.horizon_done(sid).and_then(|h| lean::lean_stats(&h, &s.result)))
                    .collect();
                endcard::render(&tracks_all, out_w as usize, out_h as usize, &c, style["title"].as_str().unwrap_or(""),
                                &credits, &lean_stats)?;
                card = Some(c);
            }
            job.set("message", "transitions, titre, musique");
            let refs: Vec<&Path> = clip_files.iter().map(PathBuf::as_path).collect();
            let info = refs.iter().map(|f| finishing::probe(f)).collect::<Result<Vec<_>>>()?;
            let (cmd, _) = finishing::finish_command(&refs, &info, &final_, style, &encoder_args(app, &q), out_w as usize,
                                                     out_h as usize, &audio, &q.audio, card.as_deref(), &credits);
            run_part_process(job, &cmd, |_| {})?;
            chapter_text = Some(montage_chapters(clips, &sessions, &parsed, style, card.is_some()));
        }
        _ => {
            let listing = out_dir.join("concat.txt");
            std::fs::write(&listing, files.iter().map(|(_, f)| format!("file '{}'\n", f.file_name().unwrap().to_string_lossy())).collect::<String>())?;
            run_checked(&["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", &listing.display().to_string(),
                          "-c", "copy", "-movflags", "+faststart", &final_.display().to_string()])?;
            if let Some(style) = &style {
                chapter_text = Some(montage_chapters(clips, &sessions, &parsed, style, false));
            }
        }
    }
    let mut done_state = json!({"state": "done", "progress": 1.0, "message": format!("terminé ({engine})"),
                                "output": final_.file_name().unwrap().to_string_lossy()});
    if let Some(text) = chapter_text {
        // chapitres YouTube à côté de l'export (même nom, .chapitres.txt)
        if !text.is_empty() {
            std::fs::write(final_.with_extension("chapitres.txt"), format!("{text}\n"))?;
        }
        done_state["chapters"] = text.into();
    }
    job.update(done_state);
    Ok(())
}

/// Chapitres YouTube d'un montage (lieu de chaque clip, transitions, carte de fin) ; texte vide
/// si YouTube les refuserait.
fn montage_chapters(clips: &[(String, Map<String, Value>)], sessions: &[Arc<crate::app::Sess>], parsed: &[Clip],
                    style: &Value, end_card: bool) -> String {
    let items: Vec<(f64, String)> = clips.iter().enumerate()
        .map(|(i, _)| {
            let c = &parsed[i];
            let keys = ramp::speed_keys(c);
            let length = if keys.is_empty() { c.end - c.start } else { ramp::output_duration(&keys, c.end - c.start, 30000.0 / 1001.0) };
            (length, telemetry::place_at(&sessions[i].result, c.start))
        })
        .collect();
    let transition = if finishing::is_plain(style) || style["transition"] == json!("aucune") {
        0.0
    } else {
        style["duration"].as_f64().unwrap_or(0.6)
    };
    let list = chapters::chapters(&items, transition, if end_card { finishing::END_CARD_S } else { 0.0 });
    chapters::description(&list)
}

// ---------------------------------------------------------------- hyperlapse

pub struct HyperOpts {
    pub duration: f64,
    pub height: u32,
    pub view: Map<String, Value>,
    pub format: String,
    pub crf: i64,
}

/// Résumé hyperlapse : toute la session en `duration` secondes, vitesse selon l'intérêt.
/// Une vue fixe (celle de l'aperçu) + l'horizon choisi ; le moteur GPU ne décode que le
/// nécessaire. Pas de son.
pub fn run_hyperlapse(app: Arc<App>, job: Arc<Job>, sid: String, opts: HyperOpts) {
    let r = hyperlapse_inner(&app, &job, &sid, opts);
    job.finish(r);
}

fn hyperlapse_inner(app: &App, job: &Job, sid: &str, opts: HyperOpts) -> Result<()> {
    if !app.gpu_engine_available() {
        bail!("le résumé hyperlapse demande le moteur GPU (render/ + NVENC)");
    }
    let sess = app.sess(sid).context("session inconnue")?;
    let (session, result) = (&sess.session, &sess.result);
    let mut o = Map::new();
    o.insert("height".into(), opts.height.into());
    o.insert("crf".into(), opts.crf.into());
    o.insert("format".into(), opts.format.clone().into());
    let q = Quality::new("final", &o);
    let (out_w, out_h) = q.output_size();
    let name = if opts.format == "standard" { format!("hyperlapse_{out_h}p") } else { format!("hyperlapse_{}", opts.format) };
    let cap = (HYPERLAPSE_MBPS_1080 * 1e6 * (out_w * out_h) as f64 / (1920.0 * 1080.0)) as u64;
    let cap = cap.min(q.max_bitrate.unwrap_or(cap));
    let settings = app.get_settings();
    let masks = settings_masks(&settings);
    let tel_opts = telemetry::Options::merged(settings["telemetry"].as_object());
    let privacy_on = settings["privacy"]["enabled"].as_bool().unwrap_or(false);
    // zones déjà analysées ou tracées dans les clips de la session, + détection image par image
    let known: Vec<Track> = if privacy_on {
        privacy::load(sid).values().flat_map(|e| bike360_core::privacy::all_tracks(Some(e))).collect()
    } else {
        vec![]
    };
    let mut view = Map::new();
    view.insert("start".into(), 0.0.into());
    view.insert("end".into(), (result.duration as f64).into());
    for (k, v) in &opts.view {
        view.insert(k.clone(), v.clone());
    }
    let full = app.horizon_done(sid);
    let hdata = full.as_deref();
    let mut vclip = to_clip(&view)?;
    if geometry::clip_horizon_mode(&vclip) == HorizonMode::Auto && hdata.is_none() {
        view.insert("horizon".into(), "fixe".into()); // analyse pas encore prête : on reste sur le support
        vclip = to_clip(&view)?;
    }
    let taus = hyperlapse::frame_times(result, opts.duration);
    let out_dir = exports_dir().join(sid).join(&name);
    std::fs::create_dir_all(&out_dir)?;
    job.set("engine", "GPU");
    let lean_track = hdata.filter(|_| tel_opts.lean).map(|h| LeanTrack::new(h, &result.tilt));
    let enc = {
        let mut q2 = q.clone();
        q2.max_bitrate = Some(cap);
        encoder_args(app, &q2)
    };
    let (mut files, mut done) = (vec![], 0usize);
    let nseg = session.segments.len();
    for (n, (seg, info)) in session.segments.iter().zip(&result.segments).enumerate() {
        let sel: Vec<f64> = taus.iter().copied()
            .filter(|t| *t >= info.offset && *t < info.offset + info.duration - 0.1).collect();
        let Some(src) = &seg.insv else { continue };
        if sel.is_empty() {
            continue;
        }
        let (fps_s, fps) = source_fps(src)?;
        // np.unique(np.round(…)) avec l'indice de la première occurrence (sel est croissant)
        let (mut samples, mut part_taus): (Vec<i64>, Vec<f64>) = (vec![], vec![]);
        for t in &sel {
            let k = ((t - info.offset) * fps).round_ties_even() as i64;
            if samples.last() != Some(&k) {
                samples.push(k);
                part_taus.push(*t);
            }
        }
        let (mut matrices, mut fovs) = (vec![], vec![]);
        for t in &part_taus {
            let v = geometry::clip_view_at(&vclip, *t);
            let m = geometry::view_matrix(v.yaw, v.pitch, level_matrix_at(&vclip, result, hdata, *t).as_ref(), v.roll);
            matrices.push(m);
            fovs.push(output_fov(v.fov, out_w, out_h));
        }
        let out = out_dir.join(format!("part_{n:03}.mp4"));
        let (h264, spec) = (out.with_extension("h264"), out.with_extension("json"));
        let mut job_spec = json!({
            "source": src, "start": 0.0, "duration": 0.0, "width": out_w, "height": out_h,
            "fov": fovs[0], "fovs": fovs, "cq": q.crf, "samples": samples, "max_bitrate": cap,
            "masks": masks, "matrices": matrices.iter().map(flat).collect::<Vec<_>>(), "output": h264,
        });
        if tel_opts.enabled && result.gps_coverage > 0.3 {
            // incrustée par le moteur
            let out_t: Vec<f64> = (0..part_taus.len()).map(|k| k as f64 / fps).collect();
            let time_map = |t: f64| interp(t, &out_t, &part_taus);
            if let Some(l) = telemetry::layers(result, Span::from(&vclip), part_taus[0], part_taus.len(), fps,
                                               out_w as usize, out_h as usize, &tel_opts, files.is_empty(),
                                               &out_dir.join(format!("tel_{n:03}")), Some(&time_map), &[],
                                               lean_track.as_ref())? {
                job_spec["sprites"] = json!(l.sprites);
                job_spec["overlays"] = json!(l.overlays);
            }
        }
        std::fs::write(&spec, serde_json::to_string(&job_spec)?)?;
        job.set("message", format!("fichier {}/{nseg} (GPU)", n + 1));
        let total = taus.len() as f64;
        let base = done;
        run_part_process(job, &[render_bin().display().to_string(), spec.display().to_string()], |line| {
            if let Some(k) = line.strip_prefix("frame=").and_then(|v| v.trim().parse::<u64>().ok()) {
                job.set("progress", ((base as f64 + k as f64) / total).min(1.0));
            }
        })?;
        let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y", "-framerate", &fps_s, "-i"].map(s).to_vec();
        cmd.push(h264.display().to_string());
        cmd.extend(["-c:v", "copy", "-movflags", "+faststart"].map(s));
        cmd.push(out.display().to_string());
        run_part_process(job, &cmd, |_| {})?;
        let _ = std::fs::remove_file(&h264);
        if privacy_on {
            job.set("message", format!("fichier {}/{nseg} : floutage visages et plaques", n + 1));
            let blurred = out.with_file_name(format!("part_{n:03}_flou.mp4"));
            let count = sel.len() as f64;
            privacy::blur_video(&out, &blurred, &part_taus, &matrices, &fovs, &known, out_w, out_h, &enc, true, job,
                                |f| job.set("progress", ((base as f64 + f * count) / total).min(1.0)))?;
            std::fs::rename(&blurred, &out)?;
        }
        done += sel.len();
        files.push(out);
    }
    if files.is_empty() {
        bail!("aucune image sélectionnée");
    }
    job.set("message", "assemblage");
    let listing = out_dir.join("concat.txt");
    std::fs::write(&listing, files.iter().map(|f| format!("file '{}'\n", f.file_name().unwrap().to_string_lossy())).collect::<String>())?;
    let final_ = exports_dir().join(format!("{sid}_{name}.mp4"));
    run_checked(&["ffmpeg", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i", &listing.display().to_string(),
                  "-c", "copy", "-movflags", "+faststart", &final_.display().to_string()])?;
    job.update(json!({"state": "done", "progress": 1.0, "message": "résumé terminé (GPU)",
                      "output": final_.file_name().unwrap().to_string_lossy()}));
    Ok(())
}

// ---------------------------------------------------------------- confidentialité et suivi

/// Tâche d'analyse de confidentialité (bouton « Analyser les clips »).
pub fn run_privacy(app: Arc<App>, job: Arc<Job>, items: Vec<(String, Map<String, Value>)>, force: bool) {
    let r = privacy::analyze(&app, &job, &items, force, "").map(|(found, skipped)| {
        let mut msg = format!("{found} zone(s) détectée(s)");
        if skipped > 0 {
            msg += &format!(", {skipped} clip(s) déjà à jour");
        }
        job.update(json!({"state": "done", "progress": 1.0, "message": msg}));
    });
    job.finish(r);
}

/// Zone tracée à la main (suivie ou fixe) : voir privacy::manual_zone.
#[allow(clippy::too_many_arguments)]
pub fn run_manual_zone(app: Arc<App>, job: Arc<Job>, sid: String, clip: Map<String, Value>, t0: f64, d0: [f64; 3], ax: f64,
                       ay: f64, track_it: bool) {
    let r = privacy::manual_zone(&app, &job, &sid, &clip, t0, d0, ax, ay, track_it)
        .map(|msg| job.update(json!({"state": "done", "progress": 1.0, "message": msg})));
    job.finish(r);
}

const FOLLOW_KEY_S: f64 = 0.5; // un point clé toutes les 0,5 s
const FOLLOW_SMOOTH_S: f64 = 0.6; // lissage des angles (pas de tremblement du cadrage)

/// Cadrage qui suit un compagnon : points clés du clip générés depuis le suivi.
#[allow(clippy::too_many_arguments)]
pub fn run_follow(app: Arc<App>, job: Arc<Job>, sid: String, clip_id: String, t0: f64, d0: [f64; 3], ax: f64, ay: f64,
                  view0: Map<String, Value>) {
    let r = follow_inner(&app, &job, &sid, &clip_id, t0, d0, ax, ay, &view0);
    job.finish(r);
}

#[allow(clippy::too_many_arguments)]
fn follow_inner(app: &App, job: &Job, sid: &str, clip_id: &str, t0: f64, d0: [f64; 3], ax: f64, ay: f64,
                view0: &Map<String, Value>) -> Result<()> {
    if !app.gpu_engine_available() {
        bail!("le suivi demande le moteur GPU (render/ + NVENC)");
    }
    let sess = app.sess(sid).context("session inconnue")?;
    let raw = app.get_selections(sid).into_iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(clip_id))
        .ok_or_else(|| anyhow!("clip introuvable"))?;
    let clip = to_clip(&raw)?;
    let track = privacy::track_sphere(app, job, sid, t0, d0, ax, ay, clip.start, clip.end)?;
    if track.is_empty() {
        bail!("suivi vide");
    }
    let full = app.horizon_done(sid);
    let ts: Vec<f64> = track.iter().map(|(t, _)| *t).collect();
    let mut yaw_r = vec![];
    let mut pitch = vec![];
    for (t, d) in &track {
        // angles de l'objet dans le repère redressé du clip
        let l = level_matrix_at(&clip, &sess.result, full.as_deref(), *t);
        let v = match l {
            Some(l) => geometry::apply(&geometry::transpose(&l), *d),
            None => *d,
        };
        yaw_r.push(v[0].atan2(v[2]));
        pitch.push(v[1].clamp(-1.0, 1.0).asin().to_degrees());
    }
    let yaw: Vec<f64> = unwrap(&yaw_r).iter().map(|v| v.to_degrees()).collect();
    // composition gardée : décalage entre l'objet et le centre de la vue au moment du tracé
    let k0 = ts.iter().enumerate().min_by(|a, b| (a.1 - t0).abs().total_cmp(&(b.1 - t0).abs())).unwrap().0;
    let g = |k: &str| view0.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let (dyaw, dpitch) = (g("yaw") - yaw[k0], g("pitch") - pitch[k0]);
    let mut keys = vec![];
    let mut t = ts[0];
    let mut i = 0;
    while t < ts[ts.len() - 1] + 1e-6 {
        let w: Vec<f64> = ts.iter().map(|x| (-((x - t) / FOLLOW_SMOOTH_S).powi(2) / 2.0).exp()).collect();
        let sw: f64 = w.iter().sum();
        let y = w.iter().zip(&yaw).map(|(a, b)| a * b).sum::<f64>() / sw + dyaw;
        let p = w.iter().zip(&pitch).map(|(a, b)| a * b).sum::<f64>() / sw + dpitch;
        keys.push(json!({"t": round_nd(t - clip.start, 2), "yaw": round_nd((y + 180.0).rem_euclid(360.0) - 180.0, 2),
                         "pitch": round_nd(p.clamp(-89.0, 89.0), 2), "roll": round_nd(g("roll"), 2),
                         "fov": round_nd(g("fov"), 1), "curve": "linear"}));
        i += 1;
        t = ts[0] + i as f64 * FOLLOW_KEY_S;
    }
    let n_keys = keys.len();
    {
        let _g = app.lock.lock().unwrap();
        let mut clips = app.get_selections(sid);
        let c = clips.iter_mut().find(|c| c.get("id").and_then(Value::as_str) == Some(clip_id)).context("clip introuvable")?;
        c.insert("keyframes".into(), Value::Array(keys));
        c.remove("roll_keys");
        c.remove("auto");
        app.write_selections(sid, &clips)?;
    }
    let span = ts[ts.len() - 1] - ts[0];
    let lost = if span >= clip.end - clip.start - 0.5 { "" } else { " — objet perdu avant la fin du clip" };
    job.update(json!({"state": "done", "progress": 1.0, "clip": clip_id, "sid": sid,
                      "message": format!("cadrage suivi sur {span:.1} s ({n_keys} points clés){lost}")}));
    Ok(())
}
