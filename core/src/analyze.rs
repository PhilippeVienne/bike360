//! Analyse des sessions : profil seconde par seconde (IMU + GPS GeoRide), synchronisation
//! automatique, score d'intérêt, moments candidats et statistiques. Résultats (même format que
//! la version Python) dans data/cache/<session>.json.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::geometry::Tilt;
use crate::insta360::{self, Session};
use crate::numeric::*;
use crate::{georide, paths};

pub const CACHE_VERSION: u32 = 5; // à incrémenter quand le calcul change (invalide le cache)
const SYNC_SEARCH_S: f64 = 120.0; // plage de recherche du décalage horloge caméra ↔ GPS
const SYNC_MIN_DURATION: usize = 300; // en dessous, corrélation peu fiable : décalage du jour
const GPS_MAX_GAP_S: f64 = 5.0; // au-delà, pas de position valide
const KNOTS_TO_KMH: f64 = 1.852;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegInfo {
    pub index: u32,
    pub lrv: String,
    pub insv: Option<String>,
    pub offset: f64,
    pub duration: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Series {
    pub speed: Vec<Option<f64>>,
    pub alt: Vec<Option<f64>>,
    pub lat: Vec<Option<f64>>,
    pub lon: Vec<Option<f64>>,
    pub turn: Vec<Option<f64>>,
    pub climb: Vec<Option<f64>>,
    pub vib: Vec<Option<f64>>,
    pub gyro: Vec<Option<f64>>,
    pub score: Vec<Option<f64>>,
}

/// Résultat d'analyse d'une session (champs supplémentaires conservés tels quels).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub id: String,
    pub date: String,
    pub time: String,
    pub key: Vec<(String, u64)>,
    #[serde(rename = "override")]
    pub override_s: Option<f64>,
    pub utc_t0: f64,
    pub offset_s: f64,
    pub offset_source: String,
    pub corr: Option<f64>,
    pub duration: usize,
    pub gps_coverage: f64,
    pub segments: Vec<SegInfo>,
    pub series: Series,
    pub candidates: Vec<usize>,
    pub version: u32,
    pub tilt: Tilt,
    pub stats: Map<String, Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Valeurs d'une série (None → NaN) pour les calculs.
pub fn values(s: &[Option<f64>]) -> Vec<f64> {
    s.iter().map(|v| v.unwrap_or(f64::NAN)).collect()
}

fn clean(a: &[f64], nd: i32) -> Vec<Option<f64>> {
    a.iter().map(|v| v.is_finite().then(|| round_nd(*v, nd))).collect()
}

// ---------------------------------------------------------------- ffprobe

pub fn ffprobe_format(path: &Path, entry: &str) -> String {
    Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", &format!("format{entry}"), "-of", "csv=p=0"])
        .arg(path)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

pub fn file_duration(path: &Path) -> f64 {
    ffprobe_format(path, "=duration").parse().unwrap_or(0.0)
}

pub fn creation_utc(path: &Path) -> Result<f64> {
    let s = ffprobe_format(path, "_tags=creation_time");
    let dt = DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00")).with_context(|| format!("date de {path:?}"))?;
    Ok(dt.timestamp_micros() as f64 / 1e6)
}

// ---------------------------------------------------------------- IMU

/// Remplit offset/durée des segments (contigus dans une session). Retourne (n secondes,
/// vibration g, rotation brute, gravité moyenne dans le repère IMU).
pub fn imu_profile(session: &mut Session) -> Result<(usize, Vec<f64>, Vec<f64>, [f64; 3])> {
    let mut parts: Vec<(Vec<f64>, Vec<f64>, Vec<f64>)> = vec![];
    let (mut acc_sum, mut acc_n) = ([0.0; 3], 0usize);
    let mut offset = 0.0;
    for seg in &mut session.segments {
        let lrv = seg.lrv.clone().context("segment sans .lrv")?;
        let imu = insta360::read_imu(&lrv)?;
        seg.offset = offset;
        seg.duration = file_duration(&lrv);
        let (mut t, mut a, mut g) = (vec![], vec![], vec![]);
        for i in 0..imu.t.len() {
            if imu.t[i] >= 0.0 && imu.t[i] < seg.duration {
                let ac = imu.acc[i];
                let gy = imu.gyro[i];
                t.push(offset + imu.t[i]);
                a.push((ac[0] * ac[0] + ac[1] * ac[1] + ac[2] * ac[2]).sqrt());
                g.push((gy[0] * gy[0] + gy[1] * gy[1] + gy[2] * gy[2]).sqrt());
                for k in 0..3 {
                    acc_sum[k] += ac[k];
                }
                acc_n += 1;
            }
        }
        parts.push((t, a, g));
        offset += seg.duration;
    }
    let last = session.segments.last().context("session vide")?;
    let n = (last.offset + last.duration).ceil() as usize;
    let (mut cnt, mut s1, mut s2, mut gs): (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) = (vec![0.0; n], vec![0.0; n], vec![0.0; n], vec![0.0; n]);
    for (t, a, g) in &parts {
        for i in 0..t.len() {
            let sec = t[i].floor();
            if sec >= 0.0 && (sec as usize) < n {
                let k = sec as usize;
                cnt[k] += 1.0;
                s1[k] += a[i];
                s2[k] += a[i] * a[i];
                gs[k] += g[i];
            }
        }
    }
    let c: Vec<f64> = cnt.iter().map(|v| v.max(1.0)).collect();
    let vib = (0..n).map(|k| (s2[k] / c[k] - (s1[k] / c[k]).powi(2)).max(0.0).sqrt()).collect();
    let gyro = (0..n).map(|k| gs[k] / c[k]).collect();
    let m = acc_n.max(1) as f64;
    Ok((n, vib, gyro, acc_sum.map(|v| v / m)))
}

/// Inclinaison fixe de la caméra (support/guidon) déduite de la gravité moyenne.
///
/// Repère caméra (x droite, y haut, z avant, repère indirect) = (+a2, −a0, +a1) dans le repère
/// IMU. Signe de x validé par symétrie : en vue ±90°, l'horizon doit être à la même hauteur des
/// deux côtés. Retourne pitch/roll (°) tels que tilt_matrix(tilt)·y = haut réel.
pub fn mount_tilt(gravity: [f64; 3]) -> Tilt {
    let n = (gravity[0].powi(2) + gravity[1].powi(2) + gravity[2].powi(2)).sqrt();
    let g = gravity.map(|v| v / n);
    Tilt {
        pitch: round_nd(-(g[1].clamp(-1.0, 1.0).asin().to_degrees()), 2),
        roll: round_nd((-g[2]).atan2(-g[0]).to_degrees(), 2),
    }
}

// ---------------------------------------------------------------- GPS

pub struct Gps {
    pub t: Vec<f64>,
    pub lat: Vec<f64>,
    pub lon: Vec<f64>,
    pub speed: Vec<f64>,
    pub alt: Vec<f64>,
    pub heading: Vec<f64>,
}

/// Positions GeoRide d'un jour UTC, en cache dans data/.
pub fn load_positions(day: NaiveDate) -> Result<Option<Gps>> {
    let path = paths::data().join(format!("georide_pos_{}.json", day.format("%Y%m%d")));
    if !path.exists() {
        let pos = georide::fetch_positions(&day.format("%Y-%m-%d").to_string(),
                                           &(day + Duration::days(1)).format("%Y-%m-%d").to_string())?;
        std::fs::create_dir_all(paths::data())?;
        std::fs::write(&path, serde_json::to_string(&pos)?)?;
    }
    let pos: Vec<Map<String, Value>> = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    if pos.is_empty() {
        return Ok(None);
    }
    let times: Vec<f64> = pos.iter()
        .map(|p| {
            let s = p.get("fixtime").and_then(Value::as_str).unwrap_or_default().replace('Z', "+00:00");
            DateTime::parse_from_rfc3339(&s).map(|d| d.timestamp_micros() as f64 / 1e6).unwrap_or(f64::NAN)
        })
        .collect();
    let mut order: Vec<usize> = (0..pos.len()).collect();
    order.sort_by(|&a, &b| times[a].total_cmp(&times[b]));
    let col = |k: &str| -> Vec<f64> {
        order.iter()
            .map(|&i| match pos[i].get(k) {
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                Some(Value::String(s)) => s.parse().unwrap_or(0.0),
                _ => 0.0,
            })
            .collect()
    };
    // GeoRide renvoie la vitesse en nœuds (vérifié contre la vitesse déduite des positions : ×1,83)
    Ok(Some(Gps {
        t: order.iter().map(|&i| times[i]).collect(),
        lat: col("latitude"),
        lon: col("longitude"),
        speed: col("speed").iter().map(|v| v * KNOTS_TO_KMH).collect(),
        alt: col("altitude"),
        heading: unwrap(&col("angle").iter().map(|v| v.to_radians()).collect::<Vec<_>>())
            .iter().map(|v| v.to_degrees()).collect(),
    }))
}

/// Positions GPS aux instants donnés (NaN hors couverture) et validité.
pub fn sample_gps(gps: &Gps, times: &[f64]) -> (HashMap<&'static str, Vec<f64>>, Vec<bool>) {
    let n = gps.t.len();
    let valid: Vec<bool> = times.iter()
        .map(|&x| {
            let idx = searchsorted(&gps.t, x).clamp(1, n - 1);
            (gps.t[idx] - x).abs().min((gps.t[idx - 1] - x).abs()) <= GPS_MAX_GAP_S
        })
        .collect();
    let mut out = HashMap::new();
    for (k, v) in [("lat", &gps.lat), ("lon", &gps.lon), ("speed", &gps.speed), ("alt", &gps.alt), ("heading", &gps.heading)] {
        out.insert(k, times.iter().zip(&valid).map(|(&x, &ok)| if ok { interp(x, &gps.t, v) } else { f64::NAN }).collect());
    }
    (out, valid)
}

/// Décalage (s) maximisant la corrélation vitesse GPS ↔ vibrations IMU : (corrélation, décalage).
pub fn auto_offset(gps: &Gps, t0: f64, vib: &[f64]) -> (f64, f64) {
    let n = vib.len();
    let mut best = (-1.0, 0.0);
    let steps = ((2.0 * SYNC_SEARCH_S) / 0.5) as i64;
    for s in 0..=steps {
        let off = -SYNC_SEARCH_S + s as f64 * 0.5;
        let times: Vec<f64> = (0..n).map(|k| t0 + k as f64 + off).collect();
        let (g, valid) = sample_gps(gps, &times);
        let nv = valid.iter().filter(|v| **v).count();
        if (nv as f64) < 0.5 * n as f64 {
            continue;
        }
        let a: Vec<f64> = (0..n).filter(|&k| valid[k]).map(|k| g["speed"][k]).collect();
        let b: Vec<f64> = (0..n).filter(|&k| valid[k]).map(|k| vib[k]).collect();
        let c = corrcoef(&a, &b);
        if c > best.0 {
            best = (c, off);
        }
    }
    best
}

// ---------------------------------------------------------------- score et statistiques

/// Score d'intérêt lissé, virages, dénivelé et moments candidats.
pub fn score_and_candidates(n: usize, vib: &[f64], gyro: &[f64], g: &HashMap<&str, Vec<f64>>, valid: &[bool])
                            -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<usize>) {
    let coverage = valid.iter().filter(|v| **v).count() as f64 / n.max(1) as f64;
    let (score, turn, climb, moving): (Vec<f64>, Vec<f64>, Vec<f64>, Vec<bool>);
    if coverage > 0.5 {
        let speed = nan_to_num(&g["speed"]);
        let mut tr: Vec<f64> = gradient(&nan_to_num(&g["heading"])).iter().map(|v| v.abs()).collect();
        for k in 0..n {
            if speed[k] < 10.0 {
                tr[k] = 0.0;
            }
        }
        turn = smooth(&tr.iter().map(|v| v.min(45.0)).collect::<Vec<_>>(), 5);
        climb = gradient(&smooth(&nan_to_num(&g["alt"]), 30)).iter().map(|v| v.abs()).collect();
        let (rt, rg, rc, rs) = (rank(&turn), rank(gyro), rank(&climb), rank(&speed));
        score = (0..n).map(|k| 0.45 * rt[k] + 0.25 * rg[k] + 0.15 * rc[k] + 0.15 * rs[k]).collect();
        moving = speed.iter().map(|v| *v > 5.0).collect();
    } else {
        turn = vec![f64::NAN; n];
        climb = vec![f64::NAN; n];
        let (rg, rv) = (rank(gyro), rank(vib));
        score = (0..n).map(|k| 0.6 * rg[k] + 0.4 * rv[k]).collect();
        let p15 = percentile(vib, 15.0);
        moving = vib.iter().map(|v| *v > p15).collect();
    }
    let score = smooth(&(0..n).map(|k| if moving[k] { score[k] } else { 0.0 }).collect::<Vec<_>>(), 15);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| score[b].partial_cmp(&score[a]).unwrap());   // np.argsort(-score)
    let mut cands: Vec<usize> = vec![];
    for k in order {
        if score[k] < 0.5 || cands.len() >= (n / 300).max(3) {
            break;
        }
        if cands.iter().all(|&c| (k as i64 - c as i64).abs() > 90) {
            cands.push(k);
        }
    }
    cands.sort();
    (score, turn, climb, cands)
}

/// Statistiques de trajet de la session (GPS).
pub fn ride_stats(n: usize, g: &HashMap<&str, Vec<f64>>, valid: &[bool]) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("duration_s".into(), n.into());
    let coverage = valid.iter().filter(|v| **v).count() as f64 / n.max(1) as f64;
    if coverage < 0.3 || n < 2 {
        return out;
    }
    let (lat, lon) = (&g["lat"], &g["lon"]);
    let mut dist = 0.0;
    for k in 1..n {
        if !(valid[k] && valid[k - 1]) {
            continue;
        }
        let (la1, la0, dl) = (lat[k].to_radians(), lat[k - 1].to_radians(), (lon[k] - lon[k - 1]).to_radians());
        let a = ((la1 - la0) / 2.0).sin().powi(2) + la1.cos() * la0.cos() * (dl / 2.0).sin().powi(2);
        let a = if a.is_finite() { a } else { 0.0 };
        let step = 2.0 * 6371000.0 * a.sqrt().asin();
        if step <= 80.0 {   // au-delà : saut GPS
            dist += step;
        }
    }
    let speed = nan_to_num(&g["speed"]);
    let moving: Vec<usize> = (0..n).filter(|&k| valid[k] && speed[k] > 3.0).collect();
    let alt_raw = &g["alt"];
    let med = nanmedian(alt_raw);
    let alt = smooth(&(0..n).map(|k| if valid[k] { if alt_raw[k].is_finite() { alt_raw[k] } else { 0.0 } } else { if med.is_finite() { med } else { 0.0 } }).collect::<Vec<_>>(), 30);
    let (mut climb, mut descent) = (0.0, 0.0);
    for k in 0..n - 1 {
        if valid[k + 1] && valid[k] {
            let d = alt[k + 1] - alt[k];
            if d > 0.0 { climb += d } else { descent -= d }
        }
    }
    let vspeed: Vec<f64> = (0..n).filter(|&k| valid[k]).map(|k| speed[k]).collect();
    let valt: Vec<f64> = (0..n).filter(|&k| valid[k] && alt_raw[k].is_finite()).map(|k| alt_raw[k]).collect();
    out.insert("distance_km".into(), round_nd(dist / 1000.0, 1).into());
    out.insert("moving_s".into(), moving.len().into());
    out.insert("avg_speed_kmh".into(), if moving.is_empty() { 0.into() } else {
        round_nd(moving.iter().map(|&k| speed[k]).sum::<f64>() / moving.len() as f64, 1).into() });
    out.insert("max_speed_kmh".into(), round_nd(percentile(&vspeed, 99.5), 1).into());
    out.insert("alt_min_m".into(), (valt.iter().cloned().fold(f64::INFINITY, f64::min) as i64).into());
    out.insert("alt_max_m".into(), (valt.iter().cloned().fold(f64::NEG_INFINITY, f64::max) as i64).into());
    out.insert("climb_m".into(), (climb as i64).into());
    out.insert("descent_m".into(), (descent as i64).into());
    out
}

// ---------------------------------------------------------------- analyse complète

fn cache_path(id: &str) -> PathBuf {
    paths::cache().join(format!("{id}.json"))
}

/// Analyse d'une session (résultat mis en cache ; recalculé si fichiers, décalage ou version changent).
/// `refs` : (utc_t0, décalage) des sessions fiables déjà analysées.
pub fn analyze(session: &mut Session, overrides: &Map<String, Value>, refs: &[(f64, f64)], force: bool) -> Result<Analysis> {
    std::fs::create_dir_all(paths::cache())?;
    let out_path = cache_path(&session.id);
    let key: Vec<(String, u64)> = session.segments.iter()
        .filter_map(|s| s.lrv.as_ref())
        .map(|p| (p.file_name().unwrap().to_string_lossy().to_string(), std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)))
        .collect();
    let override_s = overrides.get(&session.id).and_then(Value::as_f64);
    if out_path.exists() && !force {
        if let Ok(cached) = serde_json::from_str::<Analysis>(&std::fs::read_to_string(&out_path)?) {
            if cached.key == key && cached.override_s == override_s && cached.version == CACHE_VERSION {
                return Ok(cached);
            }
        }
    }

    let (n, vib, gyro, gravity) = imu_profile(session)?;
    let first_lrv = session.segments[0].lrv.clone().context("segment sans .lrv")?;
    let t0 = creation_utc(&first_lrv)?;
    let day = Utc.timestamp_opt(t0 as i64, 0).unwrap().date_naive();
    let gps = match load_positions(day) {
        Ok(g) => g,
        Err(e) => {   // pas de réseau / identifiants : on continue sans GPS
            eprintln!("  GeoRide indisponible : {e}");
            None
        }
    };

    let mut corr = None;
    let (offset, source) = if let Some(o) = override_s {
        (o, "manuel")
    } else if let (Some(g), true) = (&gps, n >= SYNC_MIN_DURATION) {
        let (c, o) = auto_offset(g, t0, &vib);
        corr = Some(c);
        (o, "corrélation")
    } else if !refs.is_empty() {
        // L'horloge caméra peut être resynchronisée (app) en cours de journée : on reprend le
        // décalage de la session fiable la plus proche dans le temps.
        let r = refs.iter().fold(&refs[0], |m, r| if (r.0 - t0).abs() < (m.0 - t0).abs() { r } else { m }); // premier minimum, comme min()
        (r.1, "session voisine")
    } else {
        (0.0, "aucun")
    };

    let times: Vec<f64> = (0..n).map(|k| t0 + offset + k as f64).collect();
    let (g, valid) = match &gps {
        Some(gp) => sample_gps(gp, &times),
        None => {
            let mut m = HashMap::new();
            for k in ["lat", "lon", "speed", "alt", "heading"] {
                m.insert(k, vec![f64::NAN; n]);
            }
            (m, vec![false; n])
        }
    };
    let (score, turn, climb, cands) = score_and_candidates(n, &vib, &gyro, &g, &valid);
    let coverage = valid.iter().filter(|v| **v).count() as f64 / n.max(1) as f64;
    let result = Analysis {
        id: session.id.clone(),
        date: session.date.clone(),
        time: session.time.clone(),
        key,
        override_s,
        utc_t0: t0,
        offset_s: offset,
        offset_source: source.into(),
        corr,
        duration: n,
        gps_coverage: round_nd(coverage, 3),
        segments: session.segments.iter()
            .map(|s| SegInfo {
                index: s.index,
                lrv: s.lrv.as_ref().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).unwrap_or_default(),
                insv: s.insv.as_ref().map(|p| p.file_name().unwrap().to_string_lossy().to_string()),
                offset: round_nd(s.offset, 3),
                duration: round_nd(s.duration, 3),
            })
            .collect(),
        series: Series {
            speed: clean(&g["speed"], 1),
            alt: clean(&g["alt"], 0),
            lat: clean(&g["lat"], 6),
            lon: clean(&g["lon"], 6),
            turn: clean(&turn, 1),
            climb: clean(&climb, 2),
            vib: clean(&vib, 3),
            gyro: clean(&gyro, 0),
            score: clean(&score, 3),
        },
        candidates: cands,
        version: CACHE_VERSION,
        tilt: mount_tilt(gravity),
        stats: ride_stats(n, &g, &valid),
        extra: Map::new(),
    };
    std::fs::write(&out_path, serde_json::to_string(&result)?)?;
    Ok(result)
}

/// Analyse de plusieurs sessions : les longues d'abord (leur décalage sert de référence aux
/// clips courts, dont la corrélation n'est pas fiable).
pub fn analyze_sessions(mut sessions: Vec<Session>, force: bool) -> Result<Vec<(Session, Analysis)>> {
    let overrides: Map<String, Value> = std::fs::read_to_string(paths::data().join("overrides.json"))
        .ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let size = |s: &Session| -> u64 {
        s.segments.iter().filter_map(|x| x.lrv.as_ref()).map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum()
    };
    sessions.sort_by_key(|s| std::cmp::Reverse(size(s)));
    let mut refs = vec![];
    let mut out = vec![];
    for mut s in sessions {
        // une session illisible (carte retirée, fichier tronqué…) est ignorée, pas fatale
        let r = match analyze(&mut s, &overrides, &refs, force) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  {} ignorée : {e:#}", s.id);
                continue;
            }
        };
        if (r.offset_source == "manuel" || r.offset_source == "corrélation") && r.corr.filter(|c| *c != 0.0).unwrap_or(1.0) > 0.5 {
            refs.push((r.utc_t0, r.offset_s));
        }
        // offsets/durées des segments (le cache ne relit pas les fichiers)
        for (seg, info) in s.segments.iter_mut().zip(&r.segments) {
            seg.offset = info.offset;
            seg.duration = info.duration;
        }
        out.push((s, r));
    }
    Ok(out)
}
