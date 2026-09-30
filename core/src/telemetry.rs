//! Incrustation de télémétrie dans les exports : vitesse, mini-carte, profil d'altitude, lieu.
//!
//! Deux chemins, même rendu :
//! - moteur GPU (render/) : [`layers`] écrit les images (PNG RGBA) et renvoie, par image de
//!   sortie, les placements `[indice, x, y, opacité]` (champs `sprites` / `overlays` du travail) ;
//! - repli ffmpeg : [`overlay`] dessine les éléments fixes en PNG, écrit un fichier sendcmd
//!   (vitesse, altitude, position du point mis à jour 10 fois par seconde) et lance ffmpeg ;
//!   [`overlay_command`] fait tout sauf lancer ffmpeg et renvoie ses arguments.
//!
//! Tailles proportionnelles au petit côté de la sortie.
//!
//! API :
//! - [`Options`] / [`DEFAULTS`] (clés « enabled, speed, map, altitude, place, lean »), [`Options::merged`].
//! - [`series`]`(r, key) -> Option<Vec<f64>>` (trous interpolés), [`raw`]`(r, key) -> Vec<f64>` (NaN),
//!   [`runs`]`(xs, ys, step)`, [`place_at`]`(r, t_session) -> String`, [`escape`]`(text)`.
//! - [`map_panel`]`(result, clip, tracks, S, U) -> (Canvas, MapProjection, attribution: bool)`.
//! - [`layers`]`(result, clip, t0, n_frames, fps, W, H, opts, first_part, workdir, time_map, tracks)
//!   -> Result<Option<Layers>>`.
//! - [`overlay`]`(part, out, result, clip, t0, dur, W, H, opts, first_part, encoder_args, workdir,
//!   time_map, tracks) -> Result<bool>` (sans jauge d'inclinaison).
//!
//! `clip` : [`Span`] (début, fin en secondes de session) ; `time_map(t_sortie) → t_session` pour
//! un temps non linéaire (hyperlapse), par défaut t0 + t ; `tracks` : analyses des sessions du
//! montage (étendue de la mini-carte, vide = la session seule).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::analyze::Analysis;
use crate::basemap::{self, Projector};
use crate::draw::{self, Canvas, ACCENT, FONT, FONT_BOLD};
use crate::lean::LeanTrack;
use crate::numeric::interp;
use crate::paths;

pub const UPDATE_HZ: f64 = 10.0;

/// Options d'incrustation (réglages du projet, mêmes clés que la version Python).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub enabled: bool,
    pub speed: bool,
    pub map: bool,
    pub altitude: bool,
    pub place: bool,
    /// Jauge d'angle d'inclinaison (horizon mesuré ; absente sans horizon).
    pub lean: bool,
}

pub const DEFAULTS: Options = Options { enabled: false, speed: true, map: true, altitude: true, place: true, lean: true };
pub const KEYS: [&str; 6] = ["enabled", "speed", "map", "altitude", "place", "lean"];

impl Default for Options {
    fn default() -> Self {
        DEFAULTS
    }
}

impl Options {
    /// {**DEFAULTS, **opts} : clés connues à valeur booléenne (vérité Python), les autres ignorées.
    pub fn merged(opts: Option<&Map<String, Value>>) -> Options {
        let mut o = DEFAULTS;
        for (k, v) in opts.into_iter().flatten() {
            let b = truthy(v);
            match k.as_str() {
                "enabled" => o.enabled = b,
                "speed" => o.speed = b,
                "map" => o.map = b,
                "altitude" => o.altitude = b,
                "place" => o.place = b,
                "lean" => o.lean = b,
                _ => {}
            }
        }
        o
    }
}

/// Valeur de vérité Python d'une valeur JSON.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Portion de session d'un clip (secondes de session ; fin −1 : aucune).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Span {
    pub start: f64,
    pub end: f64,
}

impl From<&crate::geometry::Clip> for Span {
    fn from(c: &crate::geometry::Clip) -> Self {
        Span { start: c.start, end: c.end }
    }
}

// ---------------------------------------------------------------- données

fn column<'a>(r: &'a Analysis, key: &str) -> &'a [Option<f64>] {
    let s = &r.series;
    match key {
        "speed" => &s.speed,
        "alt" => &s.alt,
        "lat" => &s.lat,
        "lon" => &s.lon,
        "turn" => &s.turn,
        "climb" => &s.climb,
        "vib" => &s.vib,
        "gyro" => &s.gyro,
        "score" => &s.score,
        _ => &[],
    }
}

/// Série brute (NaN là où le GPS manque : tunnels, pertes) pour tracer sans raccord.
pub fn raw(r: &Analysis, key: &str) -> Vec<f64> {
    column(r, key).iter().map(|v| v.unwrap_or(f64::NAN)).collect()
}

/// Série aux trous interpolés (bords prolongés), None avec moins de deux valeurs.
pub fn series(r: &Analysis, key: &str) -> Option<Vec<f64>> {
    let v = raw(r, key);
    let (xp, fp): (Vec<f64>, Vec<f64>) =
        v.iter().enumerate().filter(|(_, x)| !x.is_nan()).map(|(i, x)| (i as f64, *x)).unzip();
    if xp.len() < 2 {
        return None;
    }
    Some((0..v.len()).map(|i| interp(i as f64, &xp, &fp)).collect())
}

/// Polylignes continues (listes de points) en coupant aux NaN, sous-échantillonnées.
pub fn runs(xs: &[f64], ys: &[f64], step: usize) -> Vec<Vec<(f64, f64)>> {
    let (mut out, mut cur) = (vec![], vec![]);
    for i in (0..xs.len()).step_by(step.max(1)) {
        if xs[i].is_nan() || ys[i].is_nan() {
            if cur.len() > 1 {
                out.push(std::mem::take(&mut cur));
            }
            cur.clear();
        } else {
            cur.push((xs[i], ys[i]));
        }
    }
    if cur.len() > 1 {
        out.push(cur);
    }
    out
}

/// Commune (1er élément de l'adresse GeoRide) la plus proche dans le temps, ou ''.
pub fn place_at(r: &Analysis, t_session: f64) -> String {
    let day = Utc.timestamp_opt(r.utc_t0.floor() as i64, 0).unwrap().date_naive();
    let path = paths::data().join(format!("georide_pos_{}.json", day.format("%Y%m%d")));
    let Ok(text) = std::fs::read_to_string(&path) else { return String::new() };
    let Ok(pos) = serde_json::from_str::<Vec<Map<String, Value>>>(&text) else { return String::new() };
    let target = r.utc_t0 + r.offset_s + t_session;
    let mut best: Option<(f64, &Map<String, Value>)> = None;
    for p in &pos {
        let s = p.get("fixtime").and_then(Value::as_str).unwrap_or_default().replace('Z', "+00:00");
        let t = DateTime::parse_from_rfc3339(&s).map(|d| d.timestamp_micros() as f64 / 1e6).unwrap_or(f64::NAN);
        let d = (t - target).abs();
        if best.is_none_or(|(b, _)| d < b) {   // premier minimum, comme min()
            best = Some((d, p));
        }
    }
    let addr = best.and_then(|(_, p)| p.get("address")).and_then(Value::as_str).unwrap_or_default();
    addr.split(',').next().unwrap_or_default().to_string()
}

/// Échappement d'un texte pour drawtext (ffmpeg).
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\'', "’").replace(':', "\\:").replace('%', "\\%")
}

/// Tranche Python `v[a:b]` (indices négatifs comptés depuis la fin).
fn py_slice<T>(v: &[T], a: i64, b: i64) -> &[T] {
    let n = v.len() as i64;
    let norm = |i: i64| if i < 0 { (i + n).max(0) } else { i.min(n) };
    let (a, b) = (norm(a), norm(b));
    if a >= b { &[] } else { &v[a as usize..b as usize] }
}

fn nanmin_max(v: &[f64]) -> (f64, f64) {
    v.iter().filter(|x| !x.is_nan()).fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)))
}

fn ptp(v: &[f64]) -> f64 {
    let (a, b) = nanmin_max(v);
    b - a
}

// ---------------------------------------------------------------- mini-carte

/// Projection de la mini-carte : fond de carte (Web Mercator) ou panneau hors ligne
/// (équirectangulaire).
#[derive(Debug, Clone, Copy, Serialize)]
pub enum MapProjection {
    Tiles(Projector),
    Plain { lat0: f64, xmin: f64, ymin: f64, ptp_x: f64, ptp_y: f64, span: f64, pad: f64, size: f64 },
}

impl MapProjection {
    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        match *self {
            MapProjection::Tiles(p) => p.project(lat, lon),
            MapProjection::Plain { lat0, xmin, ymin, ptp_x, ptp_y, span, pad, size } => {
                let x = lon.to_radians() * lat0.cos();
                let y = lat.to_radians();
                (pad + (x - xmin + (span - ptp_x) / 2.0) / span * (size - 2.0 * pad),
                 size - pad - (y - ymin + (span - ptp_y) / 2.0) / span * (size - 2.0 * pad))
            }
        }
    }

    pub fn project_all(&self, lats: &[f64], lons: &[f64]) -> (Vec<f64>, Vec<f64>) {
        lats.iter().zip(lons).map(|(&a, &b)| self.project(a, b)).unzip()
    }
}

/// Mini-carte : fond de carte (ou panneau sombre hors ligne), tracés de toutes les sessions du
/// montage, session courante plus marquée, portion du clip en couleur.
///
/// Retourne (image RGBA S×S, projection, attribution requise). `u` : petit côté de la sortie.
pub fn map_panel(result: &Analysis, clip: Span, tracks: &[&Analysis], s: usize, u: f64) -> (Canvas, MapProjection, bool) {
    let own = [result];
    let tracks: &[&Analysis] = if tracks.is_empty() { &own } else { tracks };
    let lat_all: Vec<f64> = tracks.iter().flat_map(|r| raw(r, "lat")).collect();
    let lon_all: Vec<f64> = tracks.iter().flat_map(|r| raw(r, "lon")).collect();
    let radius = s as f64 * 0.08;
    let mask = draw::rounded_panel(s, s, radius, 1.0);
    let base = basemap::render(&lat_all, &lon_all, s as u32, 0.12, 1.0);
    let attribution = base.is_some();
    type Style = (([f32; 3], f32), ([f32; 3], f32), [f32; 3]);
    let (mut panel, project, styles): (Canvas, MapProjection, Style) = match base {
        Some((img, proj)) => {
            let mut panel = Canvas::new(s, s);
            for (i, p) in img.pixels().enumerate() {
                panel.px[i] = [p.0[0] as f32, p.0[1] as f32, p.0[2] as f32, mask.px[i][3]];
            }
            // fond sombre : autres balades gris clair, balade courante blanche, clip en couleur ; liseré sombre
            (panel, MapProjection::Tiles(proj), (([190.0, 196.0, 205.0], 0.7), ([255.0; 3], 0.95), [10.0, 12.0, 16.0]))
        }
        None => {   // hors ligne : ancien panneau sombre, même cadrage (équirectangulaire)
            let ok: Vec<usize> = (0..lat_all.len()).filter(|&i| !(lat_all[i].is_nan() || lon_all[i].is_nan())).collect();
            let lat0 = (ok.iter().map(|&i| lat_all[i]).sum::<f64>() / ok.len() as f64).to_radians();
            let xs: Vec<f64> = ok.iter().map(|&i| lon_all[i].to_radians() * lat0.cos()).collect();
            let ys: Vec<f64> = ok.iter().map(|&i| lat_all[i].to_radians()).collect();
            let (ptp_x, ptp_y) = (ptp(&xs), ptp(&ys));
            let span = ptp_x.max(ptp_y);
            let span = if span == 0.0 || !span.is_finite() { 1e-9 } else { span };
            let proj = MapProjection::Plain { lat0, xmin: nanmin_max(&xs).0, ymin: nanmin_max(&ys).0, ptp_x, ptp_y, span,
                                              pad: s as f64 * 0.1, size: s as f64 };
            (draw::rounded_panel(s, s, radius, 0.45), proj, (([170.0; 3], 0.6), ([210.0; 3], 0.9), [0.0; 3]))
        }
    };

    let mut layer = |runs: &[Vec<(f64, f64)>], width: f64, color: [f32; 3], alpha: f32| {
        let mut lay = Canvas::new(s, s);
        for run in runs {
            lay.stroke(run, width, color, alpha);
        }
        panel.over(&lay, 0, 0);
    };
    let ((other_c, other_a), (cur_c, cur_a), outline) = styles;
    // tracés clairs cernés de sombre : lisibles sur le relief comme sur le panneau sombre
    for r in tracks {
        if r.id != result.id {
            let (px, py) = project.project_all(&raw(r, "lat"), &raw(r, "lon"));
            let rs = runs(&px, &py, (px.len() / 600).max(1));
            layer(&rs, (u * 0.006).max(4.0), outline, 0.6);
            layer(&rs, (u * 0.003).max(2.0), other_c, other_a);
        }
    }
    let (px, py) = project.project_all(&raw(result, "lat"), &raw(result, "lon"));
    let rs = runs(&px, &py, (px.len() / 800).max(1));
    layer(&rs, (u * 0.009).max(5.0), outline, 0.7);
    layer(&rs, (u * 0.005).max(3.0), cur_c, cur_a);
    let a = clip.start as i64;
    let b = ((px.len() as f64 - 1.0).min(clip.end)) as i64;
    let clip_runs = runs(py_slice(&px, a, b + 1), py_slice(&py, a, b + 1), 1);
    layer(&clip_runs, (u * 0.016).max(8.0), [10.0, 12.0, 16.0], 0.8);   // liseré sombre sous la couleur du clip
    layer(&clip_runs, (u * 0.01).max(4.0), ACCENT, 1.0);
    for (p, m) in panel.px.iter_mut().zip(&mask.px) {
        p[3] = p[3].min(m[3]);
    }
    (panel, project, attribution)
}

// ---------------------------------------------------------------- géométrie commune

/// Dispositions communes aux deux chemins.
struct Layout {
    u: usize,
    m: usize,
    top: usize,
    bottom: usize,
    s: usize,
}

fn layout(w: usize, h: usize) -> Layout {
    let u = w.min(h);   // tailles relatives au petit côté (16:9, 1:1 ou 9:16)
    let m = (0.03 * u as f64) as usize;
    // vertical (Reels, TikTok, Shorts) : l'interface de l'appli recouvre le haut et le bas
    let top = if h > w { (0.09 * h as f64) as usize } else { m };
    let bottom = if h > w { (0.16 * h as f64) as usize } else { m };
    Layout { u, m, top, bottom, s: (0.26 * u as f64) as usize }
}

/// Profil d'altitude (panneau translucide, tracé complet, portion du clip en couleur).
fn profile(alt: &[f64], clip: Span, s: usize, ph: usize, u: f64) -> Canvas {
    let mut prof = draw::rounded_panel(s, ph, ph as f64 * 0.2, 0.45);
    let n = alt.len();
    let (lo, hi) = nanmin_max(alt);
    let (sf, phf) = (s as f64, ph as f64);
    let pt = |i: usize| (sf * 0.04 + i as f64 / (n as f64 - 1.0) * sf * 0.92,
                         phf * 0.85 - (alt[i] - lo) / (hi - lo).max(1.0) * phf * 0.55);
    let pts: Vec<(f64, f64)> = (0..n).step_by((n / 400).max(1)).map(pt).collect();
    prof.stroke(&pts, (u * 0.003).max(2.0), [220.0; 3], 0.9);
    let a = clip.start as i64;
    let b = ((n as f64 - 1.0).min(clip.end)) as i64;
    let step = ((b - a).div_euclid(100)).max(1) as usize;
    let pts: Vec<(f64, f64)> = if b >= a { (a.max(0) as usize..=b as usize).step_by(step).filter(|&i| i < n).map(pt).collect() } else { vec![] };
    prof.stroke(&pts, (u * 0.005).max(3.0), ACCENT, 1.0);
    prof
}

/// Jauge d'inclinaison : cadran (graduations tous les 15°) et aiguille penchée comme la moto.
fn lean_gauge(deg: i64, w: usize, h: usize, u: f64) -> Result<Canvas> {
    let mut img = draw::rounded_panel(w, h, h as f64 * 0.18, 0.45);
    let (cx, cy, r) = (w as f64 / 2.0, h as f64 * 0.74, h as f64 * 0.56);
    let polar = |a: f64, k: f64| (cx + r * k * a.to_radians().sin(), cy - r * k * a.to_radians().cos());
    for a in (-45..=45).step_by(15) {
        let (c, alpha) = if a == 0 { ([255.0; 3], 0.9) } else { ([200.0; 3], 0.7) };
        img.polyline(&[polar(a as f64, 0.8), polar(a as f64, 1.0)], 2f64.max(u * 0.0025), c, alpha);
    }
    img.polyline(&[polar(deg as f64, -0.12), polar(deg as f64, 0.95)], 3f64.max(u * 0.006), ACCENT, 1.0);
    img.disc(cx, cy, 3f64.max(u * 0.006), [255.0; 3], 1.0);
    let label = format!("{}°", deg.abs());
    let fs = (h as f64 * 0.2).round();
    let tw = draw::text_length(FONT_BOLD, fs, &label)?;
    draw::draw_text(&mut img, cx - tw / 2.0, cy + h as f64 * 0.02, &label, FONT_BOLD, fs, [255.0; 3], 1.0)?;
    Ok(quantize(&img))
}

fn cursor(u: usize, ph: usize) -> Canvas {
    Canvas::filled(2.max((u as f64 * 0.003) as usize), ph, [255.0, 255.0, 255.0, 230.0])
}

/// np.interp(t, arange(len(arr)), arr) sans construire l'axe.
fn at_index(arr: &[f64], t: f64) -> f64 {
    let n = arr.len();
    if n == 0 {
        return f64::NAN;
    }
    if t <= 0.0 {
        return arr[0];
    }
    if t >= (n - 1) as f64 {
        return arr[n - 1];
    }
    let j = t.floor() as usize;
    arr[j] + (arr[j + 1] - arr[j]) * (t - j as f64)
}

/// Arrondi au pair (np.round, round() Python).
fn round_even(x: f64) -> i64 {
    x.round_ties_even() as i64
}

// ---------------------------------------------------------------- moteur GPU

/// Télémétrie pour le moteur GPU : chemins des images (PNG) et, par image de sortie, les
/// placements [indice, x, y, opacité].
#[derive(Debug, Clone, Serialize)]
pub struct Layers {
    pub sprites: Vec<String>,
    pub overlays: Vec<Vec<[f64; 4]>>,
}

/// Texte sur fond transparent (couleur, ombre portée éventuelle).
fn text_sprite(text: &str, font: &str, size: usize, color: [f32; 3], shadow: bool) -> Result<Canvas> {
    Ok(draw::text_sprite(text, font, size as f64, color, shadow)?.0)
}

/// Passage en 8 bits (troncature, comme astype(uint8)) puis retour en flottant.
fn quantize(img: &Canvas) -> Canvas {
    Canvas::from_rgba8(&img.to_rgba8())
}

/// Même rendu que [`overlay`] (ffmpeg), sans réencodage : le moteur incruste pendant le rendu.
/// None sans GPS (latitude ou vitesse).
#[allow(clippy::too_many_arguments)]
pub fn layers(result: &Analysis, clip: Span, t0: f64, n_frames: usize, fps: f64, w: usize, h: usize, opts: &Options,
              first_part: bool, workdir: &Path, time_map: Option<&dyn Fn(f64) -> f64>, tracks: &[&Analysis],
              lean: Option<&LeanTrack>) -> Result<Option<Layers>> {
    let (Some(lat), Some(lon), Some(speed)) = (series(result, "lat"), series(result, "lon"), series(result, "speed")) else {
        return Ok(None);
    };
    let alt = series(result, "alt");
    std::fs::create_dir_all(workdir)?;
    let Layout { u, m, top, bottom, s } = layout(w, h);
    let uf = u as f64;
    let out_t: Vec<f64> = (0..n_frames).map(|k| k as f64 / fps).collect();
    let times: Vec<f64> = out_t.iter().map(|&t| time_map.map_or(t0 + t, |f| f(t))).collect();

    let mut paths_: Vec<String> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut sprite = |key: &str, make: &mut dyn FnMut() -> Result<Canvas>| -> Result<usize> {
        if let Some(&i) = index.get(key) {
            return Ok(i);
        }
        let img = make()?;
        let path = workdir.join(format!("s{:04}.png", paths_.len()));
        img.save_png(&path)?;
        index.insert(key.to_string(), paths_.len());
        paths_.push(path.to_string_lossy().into_owned());
        Ok(paths_.len() - 1)
    };
    let mut frames: Vec<Vec<[f64; 4]>> = vec![vec![]; n_frames];
    let place_all = |frames: &mut Vec<Vec<[f64; 4]>>, idx: usize, xs: &dyn Fn(usize) -> f64, ys: &dyn Fn(usize) -> f64,
                     alpha: Option<&[f64]>| {
        for (k, f) in frames.iter_mut().enumerate() {
            let a = alpha.map_or(1.0, |al| al[k]);
            if a > 0.0 {
                f.push([idx as f64, xs(k), ys(k), a]);
            }
        }
    };

    if opts.map {
        let (panel, project, attribution) = map_panel(result, clip, tracks, s, uf);
        let make_map = &mut || -> Result<Canvas> {
            let mut img = quantize(&panel);
            if attribution {   // attribution dessinée dans l'image (plus de drawtext)
                let txt = text_sprite(&basemap::ATTRIBUTION.join(" · "), FONT, 8.max((uf * 0.011) as usize), [220.0; 3], false)?;
                let mut bx = Canvas::filled(txt.w, txt.h, [0.0, 0.0, 0.0, 130.0]);
                bx.alpha_composite(&txt, 0, 0);
                let bx = quantize(&bx);
                img.alpha_composite(&bx, 0.max(s as i64 - bx.w as i64 - (s as f64 * 0.04) as i64),
                                    s as i64 - bx.h as i64 - (s as f64 * 0.03) as i64);
            }
            Ok(img)
        };
        let (mx, my) = ((w - m - s) as f64, top as f64);
        let i = sprite("map", make_map)?;
        place_all(&mut frames, i, &|_| mx, &|_| my, None);
        let d = 8.max((uf * 0.024) as usize) / 2 * 2;
        let (px, py) = project.project_all(&lat, &lon);
        let idx: Vec<f64> = (0..px.len()).map(|i| i as f64).collect();
        let i = sprite("dot", &mut || Ok(draw::dot(d)))?;
        let half = d as f64 / 2.0;
        place_all(&mut frames, i, &|k| mx + interp(times[k], &idx, &px) - half, &|k| my + interp(times[k], &idx, &py) - half, None);
    }
    if let (true, Some(alt)) = (opts.altitude, &alt) {
        let ph = (0.08 * uf) as usize;
        let py = (top + if opts.map { s + (0.012 * uf) as usize } else { 0 }) as f64;
        let px = (w - m - s) as f64;
        let n = alt.len();
        let i = sprite("profile", &mut || Ok(profile(alt, clip, s, ph, uf)))?;
        place_all(&mut frames, i, &|_| px, &|_| py, None);
        let i = sprite("cursor", &mut || Ok(cursor(u, ph)))?;
        let sf = s as f64;
        place_all(&mut frames, i, &|k| px + sf * 0.04 + (times[k] / (n as f64 - 1.0)).clamp(0.0, 1.0) * sf * 0.92, &|_| py, None);
        let fs = (ph as f64 * 0.3) as usize;
        for k in 0..n_frames {
            let label = format!("{} m", round_even(at_index(alt, times[k])));
            let i = sprite(&format!("alt\0{label}"), &mut || text_sprite(&label, FONT_BOLD, fs, [255.0; 3], false))?;
            frames[k].push([i as f64, px + (sf * 0.05) as usize as f64, py + (ph as f64 * 0.08) as usize as f64, 1.0]);
        }
    }
    if opts.speed {
        let (bw, bh) = ((0.22 * uf) as usize, (0.13 * uf) as usize);
        let (bx, by) = (m as f64, (h - bottom - bh) as f64);
        let i = sprite("speed_panel", &mut || {
            let mut img = quantize(&draw::rounded_panel(bw, bh, bh as f64 * 0.18, 0.45));
            let unit = text_sprite("km/h", FONT, (bh as f64 * 0.22) as usize, [230.0; 3], false)?;
            img.alpha_composite(&unit, (bw as f64 * 0.66) as i64, (bh as f64 * 0.55) as i64);
            Ok(img)
        })?;
        place_all(&mut frames, i, &|_| bx, &|_| by, None);
        let fs = (bh as f64 * 0.62) as usize;
        for k in 0..n_frames {
            let label = round_even(at_index(&speed, times[k])).max(0).to_string();
            let i = sprite(&format!("spd\0{label}"), &mut || text_sprite(&label, FONT_BOLD, fs, [255.0; 3], false))?;
            frames[k].push([i as f64, bx + (bw as f64 * 0.08) as usize as f64, by + (bh as f64 * 0.06) as usize as f64, 1.0]);
        }
    }
    if let (true, Some(track)) = (opts.lean, lean) {
        // à droite du compteur de vitesse, même hauteur ; une image par degré (créée à la demande)
        let bh = (0.13 * uf) as usize;
        let gw = (bh as f64 * 1.25) as usize;
        let gx = (m + if opts.speed { (0.22 * uf) as usize + (0.015 * uf) as usize } else { 0 }) as f64;
        let gy = (h - bottom - bh) as f64;
        for k in 0..n_frames {
            let deg = track.at(times[k]).round().clamp(-crate::lean::LIMIT_DEG, crate::lean::LIMIT_DEG) as i64;
            let i = sprite(&format!("lean\0{deg}"), &mut || lean_gauge(deg, gw, bh, uf))?;
            frames[k].push([i as f64, gx, gy, 1.0]);
        }
    }
    if opts.place && first_part {
        let place = place_at(result, clip.start);
        if !place.is_empty() {
            let i = sprite("place", &mut || text_sprite(&place, FONT_BOLD, (0.05 * uf) as usize, [255.0; 3], true))?;
            let fade: Vec<f64> = out_t.iter().map(|&t| (t / 0.6).min((4.0 - t) / 0.6).clamp(0.0, 1.0)).collect();
            place_all(&mut frames, i, &|_| m as f64, &|_| top as f64, Some(&fade));
        }
    }
    Ok(Some(Layers { sprites: paths_, overlays: frames }))
}

// ---------------------------------------------------------------- repli ffmpeg

/// Instants de mise à jour : np.arange(0, dur + 1e-6, 1 / UPDATE_HZ).
fn update_times(dur: f64) -> Vec<f64> {
    let step = 1.0 / UPDATE_HZ;
    let n = ((dur + 1e-6) / step).ceil().max(0.0) as usize;
    (0..n).map(|i| i as f64 * step).collect()
}

/// Prépare l'incrustation de `part` (morceau commençant à t0, temps de session) → `out` :
/// écrit les PNG et le fichier sendcmd dans `workdir` et renvoie la commande ffmpeg complète
/// (None sans GPS ou sans élément à incruster).
#[allow(clippy::too_many_arguments)]
pub fn overlay_command(part: &Path, out: &Path, result: &Analysis, clip: Span, t0: f64, dur: f64, w: usize, h: usize,
                       opts: &Options, first_part: bool, encoder_args: &[String], workdir: &Path,
                       time_map: Option<&dyn Fn(f64) -> f64>, tracks: &[&Analysis]) -> Result<Option<Vec<String>>> {
    let (Some(lat), Some(lon), Some(speed)) = (series(result, "lat"), series(result, "lon"), series(result, "speed")) else {
        return Ok(None);
    };
    let alt = series(result, "alt");
    std::fs::create_dir_all(workdir)?;
    let Layout { u, m, top, bottom, s } = layout(w, h);
    let uf = u as f64;
    let mut inputs: Vec<String> = vec![];
    let mut chain: Vec<String> = vec![];
    let mut label = "0:v".to_string();
    let add_image = |img: &Canvas, name: &str, inputs: &mut Vec<String>| -> Result<String> {
        let path: PathBuf = workdir.join(format!("{name}.png"));
        img.save_png(&path)?;
        inputs.extend(["-i".to_string(), path.to_string_lossy().into_owned()]);
        Ok(format!("{}:v", inputs.len() / 2))
    };
    let out_times = update_times(dur);
    let times: Vec<f64> = out_times.iter().map(|&t| time_map.map_or(t0 + t, |f| f(t))).collect();
    let mut dot_cmds: Option<Vec<(f64, f64)>> = None;
    let mut cur_cmds: Option<Vec<f64>> = None;
    let mut alt_cmds: Option<Vec<String>> = None;
    let mut spd_cmds: Option<Vec<String>> = None;

    // --- mini-carte + profil d'altitude (haut droite)
    if opts.map {
        let (panel, project, attribution) = map_panel(result, clip, tracks, s, uf);
        let (mx, my) = (w - m - s, top);
        let tag = add_image(&panel, "map", &mut inputs)?;
        chain.push(format!("[{label}][{tag}]overlay={mx}:{my}[m1]"));
        label = "m1".into();
        if attribution {
            let fs = 8.max((uf * 0.011) as usize);
            let lines = basemap::ATTRIBUTION;
            for (k, text) in lines.iter().enumerate() {
                let y = (my + s) as i64 - (s as f64 * 0.04) as i64 - (lines.len() - k) as i64 * (fs as f64 * 1.45) as i64;
                chain.push(format!("[{label}]drawtext=fontfile={FONT}:text='{}':fontsize={fs}\
                                    :fontcolor=0xdddddd:box=1:boxcolor=black@0.5:boxborderw=2\
                                    :x={}-tw:y={y}[m1a{k}]", escape(text), mx + s - (s as f64 * 0.05) as usize));
                label = format!("m1a{k}");
            }
        }
        let d = 8.max((uf * 0.024) as usize) / 2 * 2;
        let dot = add_image(&draw::dot(d), "dot", &mut inputs)?;
        let (pxx, pxy) = project.project_all(&lat, &lon);
        let idx: Vec<f64> = (0..pxx.len()).map(|i| i as f64).collect();
        let half = d as f64 / 2.0;
        let dot_x = |t: f64| mx as f64 + interp(t, &idx, &pxx) - half;
        let dot_y = |t: f64| my as f64 + interp(t, &idx, &pxy) - half;
        chain.push(format!("[{label}][{dot}]overlay@dot={:.1}:{:.1}[m2]", dot_x(t0), dot_y(t0)));
        label = "m2".into();
        dot_cmds = Some(times.iter().map(|&t| (dot_x(t), dot_y(t))).collect());
    }
    if let (true, Some(alt)) = (opts.altitude, &alt) {
        let ph = (0.08 * uf) as usize;
        let py = top + if opts.map { s + (0.012 * uf) as usize } else { 0 };
        let px = w - m - s;
        let n = alt.len();
        let tag = add_image(&profile(alt, clip, s, ph, uf), "profile", &mut inputs)?;
        chain.push(format!("[{label}][{tag}]overlay={px}:{py}[p1]"));
        label = "p1".into();
        let ctag = add_image(&cursor(u, ph), "cursor", &mut inputs)?;
        let sf = s as f64;
        let cx = |t: f64| px as f64 + sf * 0.04 + 1f64.min(0f64.max(t / (n as f64 - 1.0))) * sf * 0.92;
        chain.push(format!("[{label}][{ctag}]overlay@cur={:.1}:{py}[p2]", cx(t0)));
        label = "p2".into();
        cur_cmds = Some(times.iter().map(|&t| cx(t)).collect());
        let fs = (ph as f64 * 0.3) as usize;
        chain.push(format!("[{label}]drawtext@alt=fontfile={FONT_BOLD}:text='{} m':fontsize={fs}\
                            :fontcolor=white:x={}:y={}[p3]", at_index(alt, t0) as i64, px + (sf * 0.05) as usize,
                           py + (ph as f64 * 0.08) as usize));
        label = "p3".into();
        alt_cmds = Some(times.iter().map(|&t| format!("{} m", round_even(at_index(alt, t)))).collect());
    }

    // --- vitesse (bas gauche)
    if opts.speed {
        let (bw, bh) = ((0.22 * uf) as usize, (0.13 * uf) as usize);
        let tag = add_image(&draw::rounded_panel(bw, bh, bh as f64 * 0.18, 0.45), "speed", &mut inputs)?;
        let (bx, by) = (m, h - bottom - bh);
        chain.push(format!("[{label}][{tag}]overlay={bx}:{by}[s1]"));
        let fs = (bh as f64 * 0.62) as usize;
        chain.push(format!("[s1]drawtext@spd=fontfile={FONT_BOLD}:text='{}':fontsize={fs}\
                            :fontcolor=white:x={}:y={}[s2]", round_even(at_index(&speed, t0)),
                           bx + (bw as f64 * 0.08) as usize, by + (bh as f64 * 0.12) as usize));
        chain.push(format!("[s2]drawtext=fontfile={FONT}:text='km/h':fontsize={}:fontcolor=white@0.85\
                            :x={}:y={}[s3]", (bh as f64 * 0.22) as usize, bx + (bw as f64 * 0.66) as usize,
                           by + (bh as f64 * 0.55) as usize));
        label = "s3".into();
        spd_cmds = Some(times.iter().map(|&t| round_even(at_index(&speed, t)).to_string()).collect());
    }

    // --- lieu au début du clip (fondu)
    if opts.place && first_part {
        let place = place_at(result, clip.start);
        if !place.is_empty() {
            let fade = "if(lt(t,0.6),t/0.6,if(lt(t,3.4),1,max(0,(4-t)/0.6)))";
            chain.push(format!("[{label}]drawtext=fontfile={FONT_BOLD}:text='{}':fontsize={}\
                                :fontcolor=white:alpha='{fade}':shadowcolor=black@0.6:shadowx=2:shadowy=2\
                                :x={m}:y={top}:enable='lt(t,4)'[l1]", escape(&place), (0.05 * uf) as usize));
            label = "l1".into();
        }
    }

    if chain.is_empty() {
        return Ok(None);
    }
    // commandes de mise à jour
    let mut lines = vec![];
    for k in 0..times.len() {
        let mut parts = vec![];
        if let Some(c) = &dot_cmds {
            parts.push(format!("overlay@dot x {:.1}", c[k].0));
            parts.push(format!("overlay@dot y {:.1}", c[k].1));
        }
        if let Some(c) = &cur_cmds {
            parts.push(format!("overlay@cur x {:.1}", c[k]));
        }
        if let Some(c) = &alt_cmds {
            parts.push(format!("drawtext@alt reinit text={}", c[k].replace(' ', "\u{a0}")));
        }
        if let Some(c) = &spd_cmds {
            parts.push(format!("drawtext@spd reinit text={}", c[k]));
        }
        if !parts.is_empty() {
            lines.push(format!("{:.2} {};", out_times[k], parts.join(", ")));
        }
    }
    let cmdfile = workdir.join("telemetry.cmd");
    std::fs::write(&cmdfile, lines.join("\n") + "\n")?;
    let graph = format!("[0:v]sendcmd=f='{}'[v0];{};[{label}]format=yuv420p[vout]", cmdfile.display(),
                        chain.join(";").replacen("[0:v]", "[v0]", 1));
    let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y", "-i"].map(String::from).to_vec();
    cmd.push(part.to_string_lossy().into_owned());
    cmd.extend(inputs);
    cmd.extend(["-filter_complex".to_string(), graph, "-map".into(), "[vout]".into(), "-map".into(), "0:a?".into()]);
    cmd.extend(encoder_args.iter().cloned());
    cmd.extend(["-c:a", "copy", "-movflags", "+faststart"].map(String::from));
    cmd.push(out.to_string_lossy().into_owned());
    Ok(Some(cmd))
}

/// Incruste la télémétrie sur `part` → `out` (ffmpeg). Faux sans GPS ou sans élément.
#[allow(clippy::too_many_arguments)]
pub fn overlay(part: &Path, out: &Path, result: &Analysis, clip: Span, t0: f64, dur: f64, w: usize, h: usize,
               opts: &Options, first_part: bool, encoder_args: &[String], workdir: &Path,
               time_map: Option<&dyn Fn(f64) -> f64>, tracks: &[&Analysis]) -> Result<bool> {
    let Some(cmd) = overlay_command(part, out, result, clip, t0, dur, w, h, opts, first_part, encoder_args, workdir,
                                    time_map, tracks)? else {
        return Ok(false);
    };
    run(&cmd)?;
    Ok(true)
}

/// Lance une commande (arguments complets) ; erreur avec sa sortie d'erreur si elle échoue.
pub fn run(cmd: &[String]) -> Result<()> {
    let o = Command::new(&cmd[0]).args(&cmd[1..]).output()?;
    if !o.status.success() {
        bail!("{} a échoué : {}", cmd[0], String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}
