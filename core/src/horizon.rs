//! Horizon mesuré dans l'image : la verticale est le point de fuite des contours verticaux.
//!
//! Toutes les lignes verticales du monde (troncs, poteaux, murs, arêtes) passent par le
//! zénith sur la sphère : chaque contour définit un grand cercle de normale n et le « haut »
//! réel u vérifie n·u = 0.
//!
//! Estimation robuste en trois parties :
//! 1. image : score de chaque orientation candidate (grille roulis × tangage autour de
//!    l'inclinaison fixe de la caméra) = somme pondérée des contours compatibles ;
//! 2. a priori physique : roulis centré sur l'inclinaison déduite du GPS, d'autant plus
//!    serré que la vitesse est élevée (les grands angles de guidon n'arrivent qu'au pas) ;
//! 3. continuité : chemin le plus probable par Viterbi (pas de sauts isolés).
//!
//! Ce calcul remplace l'IMU : avec une caméra au guidon, le gyroscope n'a pas pu être
//! exploité de façon fiable. Pièges connus : marquages de route, glissières et tunnels (lignes
//! fuyantes), d'où l'a priori de vitesse (vitesse interpolée quand le GPS décroche).

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::Result;
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::analyze::{self, Analysis};
use crate::geometry::{self, Mat3};
use crate::insta360::Session;
use crate::numeric::{gauss, gradient, interp, round_nd};

pub const HZ: usize = 10; // estimations par seconde de vidéo
const W: usize = 1024; // image équirectangulaire d'analyse
const H: usize = 512;
const MAX_EDGES: usize = 3000; // contours retenus par image (les plus contrastés)
const EMISSION_WEIGHT: f32 = 4.0;
const TRANSITION_SIGMA: f64 = 3.0; // ° par pas de 0,1 s
pub const CACHE_VERSION: u32 = 6; // 6 : vitesse GeoRide convertie des nœuds en km/h (a priori)
const SIGMA: f32 = 0.04;

/// Roulis résiduel candidat (°) par rapport à l'inclinaison fixe : −44..44 par pas de 2.
pub fn rolls() -> Vec<f64> {
    (0..45).map(|k| -44.0 + 2.0 * k as f64).collect()
}

/// Tangage résiduel candidat (°) : −18..18 par pas de 3.
pub fn pitches() -> Vec<f64> {
    (0..13).map(|k| -18.0 + 3.0 * k as f64).collect()
}

/// Haut réel (repère de l'image équirect / caméra redressée de l'inclinaison fixe).
pub fn up_from_angles(roll_deg: f64, pitch_deg: f64) -> [f64; 3] {
    let (r, p) = (roll_deg.to_radians(), pitch_deg.to_radians());
    [-r.sin() * p.cos(), r.cos() * p.cos(), p.sin()]
}

/// États (roulis, tangage), roulis en indice lent.
static STATES: LazyLock<Vec<(f64, f64)>> =
    LazyLock::new(|| rolls().iter().flat_map(|r| pitches().into_iter().map(move |p| (*r, p))).collect());

/// Le repère x droite / y haut / z avant est indirect : pour la géométrie des contours,
/// le haut s'exprime en miroir x/z (validé visuellement sur des scènes de référence).
static GEOMETRIC: LazyLock<Vec<[f32; 3]>> = LazyLock::new(|| {
    STATES.iter()
        .map(|(r, p)| {
            let u = up_from_angles(*r, *p);
            [-u[0] as f32, u[1] as f32, -u[2] as f32]
        })
        .collect()
});

pub fn n_states() -> usize {
    STATES.len()
}

/// Normales des grands cercles portés par les contours (hors moto en bas et pilote à l'arrière).
///
/// `exclude` : masque (H×W) des pixels fixes par rapport à la caméra (guidon, rétroviseurs…),
/// dont les contours voteraient pour « aucune correction ».
pub fn edge_normals(img: &[f32], exclude: Option<&[bool]>) -> (Vec<[f32; 3]>, Vec<f32>) {
    let mut cand: Vec<(usize, usize, f32, f32, f32)> = vec![];
    for r in (1..H - 1).step_by(2) {
        let lat = std::f64::consts::FRAC_PI_2 - (r as f64 + 0.5) / H as f64 * std::f64::consts::PI;
        let lat_deg = lat.to_degrees();
        if !(lat_deg > -15.0 && lat_deg < 55.0) {
            continue;
        }
        for c in (1..W - 1).step_by(2) {
            let lon = (c as f64 + 0.5) / W as f64 * 2.0 * std::f64::consts::PI - std::f64::consts::PI;
            let lon_deg = (lon.to_degrees() + 360.0) % 360.0;
            if lon_deg > 125.0 && lon_deg < 235.0 {
                continue;
            }
            if exclude.is_some_and(|m| m[r * W + c]) {
                continue;
            }
            let gx = (img[r * W + c + 1] - img[r * W + c - 1]) / 2.0;
            let gy = (img[(r + 1) * W + c] - img[(r - 1) * W + c]) / 2.0;
            let mag = gx.hypot(gy);
            if mag > 12.0 {
                cand.push((r, c, gx, gy, mag));
            }
        }
    }
    if cand.len() > MAX_EDGES {
        cand.select_nth_unstable_by(MAX_EDGES, |a, b| b.4.total_cmp(&a.4));
        cand.truncate(MAX_EDGES);
    }
    let mut normals = Vec::with_capacity(cand.len());
    let mut weights = Vec::with_capacity(cand.len());
    for (r, c, ax, ay, w) in cand {
        let lt = std::f64::consts::FRAC_PI_2 - (r as f64 + 0.5) / H as f64 * std::f64::consts::PI;
        let ln = (c as f64 + 0.5) / W as f64 * 2.0 * std::f64::consts::PI - std::f64::consts::PI;
        let (ax, ay) = (ax as f64, ay as f64);
        let d = [lt.cos() * ln.sin(), lt.sin(), lt.cos() * ln.cos()];
        let e_lon = [ln.cos(), 0.0, -ln.sin()];
        let e_lat = [-lt.sin() * ln.sin(), lt.cos(), -lt.sin() * ln.cos()];
        // contour ⟂ gradient ; y image vers le bas
        let t: [f64; 3] = [0, 1, 2].map(|k| -ay * e_lon[k] - ax * e_lat[k]);
        let n = [d[1] * t[2] - d[2] * t[1], d[2] * t[0] - d[0] * t[2], d[0] * t[1] - d[1] * t[0]];
        let norm = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt() + 1e-9;
        normals.push(n.map(|v| (v / norm) as f32));
        weights.push(w.min(60.0));
    }
    (normals, weights)
}

/// Log-score (≤ 0) de chaque état : poids des contours compatibles avec ce « haut ».
pub fn emission(img: &[f32], exclude: Option<&[bool]>) -> Vec<f32> {
    let (n, w) = edge_normals(img, exclude);
    if w.is_empty() {
        return vec![0.0; n_states()];
    }
    let s: Vec<f32> = GEOMETRIC.iter()
        .map(|g| {
            n.iter().zip(&w)
                .map(|(v, w)| {
                    let x = (g[0] * v[0] + g[1] * v[1] + g[2] * v[2]) / SIGMA;
                    (-x * x).exp() * w
                })
                .sum()
        })
        .collect();
    let top = s.iter().cloned().fold(f32::MIN, f32::max);
    s.iter().map(|v| (v / top + 1e-6).ln()).collect()
}

/// Log-a priori (T×S) : roulis autour du centre donné, écart permis décroissant avec la vitesse.
pub fn speed_prior(speed_kmh: &[f64], roll_center_deg: &[f64]) -> Vec<f32> {
    let mut out = Vec::with_capacity(speed_kmh.len() * n_states());
    for (v, c) in speed_kmh.iter().zip(roll_center_deg) {
        let sig = 8.0 + 25.0 * (-v / 30.0).exp();
        for (r, p) in STATES.iter() {
            out.push((-((r - c).powi(2)) / (2.0 * sig * sig) - p * p / (2.0 * 10.0 * 10.0)) as f32);
        }
    }
    out
}

/// Chemin d'états le plus probable ; transitions vers les états voisins (±6° roulis, ±6° tangage).
/// `e` et `prior` : T×S à plat.
pub fn viterbi(e: &[f32], prior: &[f32]) -> Vec<usize> {
    let (nr, np) = (rolls().len(), pitches().len());
    let s = nr * np;
    let t_len = e.len() / s;
    if t_len == 0 {
        return vec![];
    }
    let mut score: Vec<f32> = (0..s).map(|k| EMISSION_WEIGHT * e[k] + prior[k]).collect();
    let mut back = vec![0u32; t_len * s];
    let mut moves: Vec<(i64, i64, f32)> = vec![];
    for a in -3i64..=3 {
        for b in -2i64..=2 {
            let pen = -(((a as f64 * 2.0).powi(2) + (b as f64 * 3.0).powi(2)) / (2.0 * TRANSITION_SIGMA * TRANSITION_SIGMA));
            moves.push((a, b, pen as f32));
        }
    }
    let mut best = vec![0f32; s];
    for t in 1..t_len {
        best.fill(f32::NEG_INFINITY);
        let arg = &mut back[t * s..(t + 1) * s];
        arg.fill(0);
        for &(a, b, pen) in &moves {
            for ri in 0..nr as i64 {
                let sr = ri - a;
                if sr < 0 || sr >= nr as i64 {
                    continue;
                }
                for pi in 0..np as i64 {
                    let sp = pi - b;
                    if sp < 0 || sp >= np as i64 {
                        continue;
                    }
                    let (dst, src) = ((ri * np as i64 + pi) as usize, (sr * np as i64 + sp) as usize);
                    let cand = score[src] + pen;
                    if cand > best[dst] {
                        best[dst] = cand;
                        arg[dst] = src as u32;
                    }
                }
            }
        }
        for k in 0..s {
            score[k] = best[k] + EMISSION_WEIGHT * e[t * s + k] + prior[t * s + k];
        }
    }
    let mut path = vec![0usize; t_len];
    // premier maximum, comme np.argmax
    path[t_len - 1] = (0..s).fold(0, |m, k| if score[k] > score[m] { k } else { m });
    for t in (1..t_len).rev() {
        path[t - 1] = back[t * s + path[t]] as usize;
    }
    path
}

/// Vitesse (km/h, pertes GPS comblées) et inclinaison GPS (°) à HZ sur la session.
pub fn gps_prior_inputs(result: &Analysis, n_total: usize) -> (Vec<f64>, Vec<f64>) {
    let fallback = (vec![30.0; n_total], vec![0.0; n_total]);
    let t: Vec<f64> = (0..n_total).map(|k| k as f64 / HZ as f64).collect();
    let day = Utc.timestamp_opt(result.utc_t0 as i64, 0).unwrap().date_naive();
    let Ok(Some(gps)) = analyze::load_positions(day) else { return fallback };
    let times: Vec<f64> = t.iter().map(|x| result.utc_t0 + result.offset_s + x).collect();
    let (g, valid) = analyze::sample_gps(&gps, &times);
    let tv: Vec<f64> = (0..n_total).filter(|&k| valid[k]).map(|k| t[k]).collect();
    if tv.len() < 2 {
        return fallback;
    }
    let sv: Vec<f64> = (0..n_total).filter(|&k| valid[k]).map(|k| g["speed"][k]).collect();
    let hv: Vec<f64> = (0..n_total).filter(|&k| valid[k]).map(|k| g["heading"][k]).collect();
    let speed: Vec<f64> = t.iter().map(|x| interp(*x, &tv, &sv)).collect(); // tunnels : interpolation
    let heading: Vec<f64> = t.iter().map(|x| interp(*x, &tv, &hv)).collect();
    let yaw_rate = gauss(&gradient(&heading).iter().map(|v| v * HZ as f64).collect::<Vec<_>>(), 15.0);
    let lean = speed.iter().zip(&yaw_rate)
        .map(|(v, y)| (v / 3.6 * y.to_radians() / 9.81).atan().to_degrees())
        .collect();
    (speed, lean)
}

/// Moteur CUDA : à côté de l'exécutable courant, sinon dans target/release du dépôt.
fn render_bin() -> PathBuf {
    std::env::current_exe().ok()
        .and_then(|p| Some(p.parent()?.join("insta-render")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/release/insta-render")))
}

/// Émissions calculées par le moteur CUDA (render/) ; None si indisponible ou en échec.
pub fn emissions_gpu(lrv: &Path, start: f64, duration: f64, base: &Mat3, progress: Option<&dyn Fn(f64)>) -> Option<Vec<f32>> {
    let bin = render_bin();
    if !bin.exists() {
        return None;
    }
    let tmp = tempdir().ok()?;
    let out = tmp.join("scores.f32");
    let spec = tmp.join("job.json");
    let job = json!({
        "source": lrv, "start": start, "duration": duration, "hz": HZ,
        "base": base.iter().flatten().collect::<Vec<_>>(),
        "states": *GEOMETRIC, "sigma": SIGMA, "width": W, "height": H,
        "lat_min": -15.0, "lat_max": 55.0, "excl_lon": [125.0, 235.0], "grad_min": 12.0, "weight_cap": 60.0,
        "output": out,
    });
    let res = (|| -> Option<Vec<f32>> {
        std::fs::write(&spec, job.to_string()).ok()?;
        let mut proc = Command::new(&bin).arg("horizon").arg(&spec)
            .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let n_out = (duration * HZ as f64) as usize;
        for line in BufReader::new(proc.stdout.take()?).lines().map_while(Result::ok) {
            if let (Some(p), Some(n)) = (progress, line.strip_prefix("frame=")) {
                p((n.trim().parse::<f64>().unwrap_or(0.0) / n_out.max(1) as f64).min(1.0));
            }
        }
        if !proc.wait().ok()?.success() {
            return None;
        }
        let raw = std::fs::read(&out).ok()?;
        let s: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        let ns = n_states();
        let mut e = Vec::with_capacity(s.len());
        for row in s.chunks_exact(ns) {
            let top = row.iter().cloned().fold(f32::MIN, f32::max);
            e.extend(row.iter().map(|v| if top > 0.0 { (v / top.max(1e-12) + 1e-6).ln() } else { 0.0 }));
        }
        Some(e)
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

fn tempdir() -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("insta-horizon-{}-{}", std::process::id(),
                                                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Émissions par ffmpeg (v360) et calcul sur processeur, quand le moteur CUDA est absent.
fn emissions_cpu(lrv: &Path, start: Option<(f64, f64)>, base: &Mat3, progress: Option<(&dyn Fn(f64), f64)>) -> Vec<f32> {
    let (yaw, pitch, roll) = geometry::v360_angles(base);
    let vf = format!("fps={HZ},v360=input=dfisheye:ih_fov=195:iv_fov=195:output=equirect\
                      :yaw={yaw:.3}:pitch={pitch:.3}:roll={roll:.3}:w={W}:h={H},format=gray");
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if let Some((ss, t)) = start {
        cmd.args(["-ss", &format!("{ss:.3}"), "-t", &format!("{t:.3}")]);
    }
    cmd.arg("-i").arg(lrv).args(["-vf", &vf, "-f", "rawvideo", "-"]).stdout(Stdio::piped());
    let mut out = vec![];
    let Ok(mut proc) = cmd.spawn() else { return out };
    let mut stdout = proc.stdout.take().unwrap();
    let mut buf = vec![0u8; W * H];
    let mut frames = 0usize;
    while stdout.read_exact(&mut buf).is_ok() {
        let img: Vec<f32> = buf.iter().map(|v| *v as f32).collect();
        out.extend(emission(&img, None));
        frames += 1;
        if let Some((p, duration)) = progress {
            if frames % HZ == 0 {
                p((frames as f64 / HZ as f64 / duration).min(1.0));
            }
        }
    }
    let _ = proc.wait();
    out
}

pub fn segment_emissions(lrv: &Path, duration: f64, base: &Mat3, progress: Option<&dyn Fn(f64)>) -> Vec<f32> {
    if let Some(e) = emissions_gpu(lrv, 0.0, duration, base, progress) {
        return e;
    }
    emissions_cpu(lrv, None, base, progress.map(|p| (p, duration)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HorizonData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Value>,
    pub hz: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t0: Option<f64>,
    pub up: Vec<[f64; 3]>,
}

/// Roulis/tangage lissés du chemin de Viterbi → haut réel dans le repère caméra (arrondi).
fn path_to_up(states: &[usize], base: &Mat3) -> Vec<[f64; 3]> {
    let (r, p) = (rolls(), pitches());
    let np = p.len();
    // efface les paliers de la grille
    let roll = gauss(&states.iter().map(|s| r[s / np]).collect::<Vec<_>>(), 1.5);
    let pitch = gauss(&states.iter().map(|s| p[s % np]).collect::<Vec<_>>(), 1.5);
    roll.iter().zip(&pitch)
        .map(|(r, p)| geometry::apply(base, up_from_angles(*r, *p)).map(|v| round_nd(v, 4)))
        .collect()
}

/// Horizon d'une session entière (temps de session, HZ), mis en cache.
pub fn compute(session: &Session, result: &Analysis, cache_dir: &Path, progress: Option<&dyn Fn(f64)>) -> Result<HorizonData> {
    let path = cache_dir.join(format!("{}_horizon.json", session.id));
    let key = json!([result.key, result.offset_s, CACHE_VERSION]);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cached) = serde_json::from_str::<HorizonData>(&text) {
            if cached.key.as_ref() == Some(&key) {
                return Ok(cached);
            }
        }
    }
    let base = geometry::tilt_matrix(Some(&result.tilt));
    let total: f64 = result.segments.iter().map(|s| s.duration).sum();
    let n_total = (total * HZ as f64).ceil() as usize;
    let ns = n_states();
    let mut e = vec![0f32; n_total * ns];
    let mut done = 0.0;
    for (seg, info) in session.segments.iter().zip(&result.segments) {
        let Some(lrv) = &seg.lrv else { continue };
        let d0 = done;
        let seg_progress = |f: f64| {
            if let Some(p) = progress {
                p(0.95 * (d0 + f * info.duration) / total);
            }
        };
        let es = segment_emissions(lrv, info.duration, &base, Some(&seg_progress));
        let k0 = ((info.offset * HZ as f64).round_ties_even() as usize).min(n_total);
        let k1 = n_total.min(k0 + es.len() / ns);
        e[k0 * ns..k1 * ns].copy_from_slice(&es[..(k1 - k0) * ns]);
        done += info.duration;
    }
    let (speed, lean) = gps_prior_inputs(result, n_total);
    let neg: Vec<f64> = lean.iter().map(|v| -v).collect();
    let states = viterbi(&e, &speed_prior(&speed, &neg));
    // Pas d'indicateur de fiabilité : le contraste des scores ne distingue pas les scènes
    // faciles des difficiles (tunnel ≈ virages), il serait trompeur.
    let data = HorizonData { key: Some(key), hz: HZ, t0: None, up: path_to_up(&states, &base) };
    let mut v = serde_json::to_value(&data)?;
    v["reliable"] = Value::Null;
    std::fs::write(&path, v.to_string())?;
    if let Some(p) = progress {
        p(1.0);
    }
    Ok(data)
}

/// Horizon d'une portion de session seulement (export sans attendre l'analyse complète).
pub fn compute_range(session: &Session, result: &Analysis, t_start: f64, t_end: f64) -> Option<HorizonData> {
    let base = geometry::tilt_matrix(Some(&result.tilt));
    let t_start = t_start.max(0.0);
    let ns = n_states();
    let mut e: Vec<f32> = vec![];
    for (seg, info) in session.segments.iter().zip(&result.segments) {
        let Some(lrv) = &seg.lrv else { continue };
        let a = t_start.max(info.offset);
        let b = t_end.min(info.offset + info.duration);
        if b - a <= 0.0 {
            continue;
        }
        match emissions_gpu(lrv, a - info.offset, b - a, &base, None) {
            Some(g) => e.extend(g),
            None => e.extend(emissions_cpu(lrv, Some((a - info.offset, b - a)), &base, None)),
        }
    }
    let n = e.len() / ns;
    if n < 2 {
        return None;
    }
    let n_total = (t_start * HZ as f64).ceil() as usize + n;
    let (speed, lean) = gps_prior_inputs(result, n_total);
    let k0 = n_total - n;
    let neg: Vec<f64> = lean[k0..].iter().map(|v| -v).collect();
    let states = viterbi(&e, &speed_prior(&speed[k0..], &neg));
    Some(HorizonData { key: None, hz: HZ, t0: Some(k0 as f64 / HZ as f64), up: path_to_up(&states, &base) })
}

/// Matrice de redressement à l'instant t (temps de session), ou None.
pub fn level_at(data: Option<&HorizonData>, t: f64) -> Option<Mat3> {
    let data = data?;
    if data.up.is_empty() {
        return None;
    }
    let up = &data.up;
    let x = ((t - data.t0.unwrap_or(0.0)) * data.hz as f64).clamp(0.0, (up.len() - 1) as f64);
    let i = x.floor() as usize;
    let j = (i + 1).min(up.len() - 1);
    let f = x - i as f64;
    Some(geometry::min_rotation([0, 1, 2].map(|k| up[i][k] * (1.0 - f) + up[j][k] * f)))
}
