//! Montage automatique : les meilleurs moments des sessions du projet, pour une durée cible.
//!
//! Sur la courbe d'intérêt de chaque session (analyze : virages, rotation, dénivelé, vitesse),
//! on retient des pics espacés d'au moins MIN_GAP_S ; chaque clip s'étend tant que l'intérêt
//! reste proche du pic (6 à 14 s). Les sessions reçoivent une part proportionnelle à leur
//! durée, pour ne pas tout prendre dans le même col. Les clips existants sont évités.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use serde_json::Map;

use crate::analyze::Analysis;
use crate::geometry::Clip;
use crate::numeric::round_nd;

const MIN_GAP_S: i64 = 60;
const CLIP_MIN_S: f64 = 6.0;
const CLIP_MAX_S: f64 = 14.0;
const EDGE_S: usize = 20; // ni tout début ni toute fin de session (démarrage, arrêt)
const PEAK_MIN: f64 = 0.55; // intérêt minimal d'un pic (score classé 0..1)
const KEEP_RATIO: f64 = 0.85; // le clip couvre la zone où l'intérêt reste ≥ 85 % du pic
const MIN_SESSION_S: usize = 120; // sessions plus courtes ignorées (essais, arrêts)

fn score(result: &Analysis) -> Vec<f64> {
    result.series.score.iter().map(|v| v.unwrap_or(0.0)).collect()
}

/// Pics d'intérêt [(t, score)] espacés d'au moins MIN_GAP_S, du plus fort au plus faible.
pub fn peaks(result: &Analysis) -> Vec<(usize, f64)> {
    let s = score(result);
    let n = s.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| s[b].partial_cmp(&s[a]).unwrap());
    let mut out: Vec<(usize, f64)> = vec![];
    for k in order {
        if s[k] < PEAK_MIN {
            break;
        }
        if EDGE_S <= k && k + EDGE_S < n && out.iter().all(|(t, _)| (k as i64 - *t as i64).abs() > MIN_GAP_S) {
            out.push((k, s[k]));
        }
    }
    out
}

/// Clip autour du pic t : étendu tant que l'intérêt reste proche du pic.
pub fn window(result: &Analysis, t: usize) -> (f64, f64) {
    let s = score(result);
    let thr = KEEP_RATIO * s[t];
    let (mut a, mut b) = (t, t);
    while (a as f64) > t as f64 - CLIP_MAX_S / 2.0 && a > 0 && s[a - 1] >= thr {
        a -= 1;
    }
    while (b as f64) < t as f64 + CLIP_MAX_S / 2.0 && b + 1 < s.len() && s[b + 1] >= thr {
        b += 1;
    }
    let length = CLIP_MAX_S.min(CLIP_MIN_S.max((b - a) as f64));
    let start = 0f64.max((s.len() as f64 - length).min((a + b) as f64 / 2.0 - length / 2.0));
    (round_nd(start, 2), round_nd(start + length, 2))
}

/// Identifiant aléatoire de clip (8 caractères hexadécimaux).
pub fn new_id() -> String {
    let mut b = [0u8; 4];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        b = (t as u32).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn auto_clip(start: f64, end: f64) -> Clip {
    Clip {
        id: Some(new_id()), start, end, yaw: 0.0, pitch: -10.0, roll: 0.0, fov: 100.0,
        horizon: Some("fixe".into()), level: None, keyframes: None, roll_keys: None, auto: Some(true),
        extra: Map::new(),
    }
}

/// Clips à ajouter {sid: [clip]} pour atteindre ~target_s de montage.
///
/// `results` : analyses des sessions du projet ; `existing` : clips à éviter par session.
pub fn plan(results: &BTreeMap<String, Analysis>, target_s: f64, existing: &HashMap<String, Vec<Clip>>, transition_s: f64)
            -> BTreeMap<String, Vec<Clip>> {
    let sessions: BTreeMap<&String, &Analysis> = results.iter().filter(|(_, r)| r.duration >= MIN_SESSION_S).collect();
    if sessions.is_empty() {
        return BTreeMap::new();
    }
    let total: f64 = sessions.values().map(|r| r.duration as f64).sum();
    let quota: HashMap<&String, f64> = sessions.iter().map(|(sid, r)| (*sid, target_s * r.duration as f64 / total)).collect();
    let mut cands: Vec<(f64, &String, usize)> =
        sessions.iter().flat_map(|(sid, r)| peaks(r).into_iter().map(move |(t, sc)| (sc, *sid, t))).collect();
    // tri décroissant sur (score, session, instant), comme sorted(..., reverse=True)
    cands.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap().then_with(|| y.1.cmp(x.1)).then_with(|| y.2.cmp(&x.2)));
    let mut chosen: HashMap<&String, Vec<Clip>> = sessions.keys().map(|sid| (*sid, vec![])).collect();
    let mut used: HashMap<&String, f64> = sessions.keys().map(|sid| (*sid, 0.0)).collect();
    let mut length = 0.0;
    let empty = vec![];
    for relax in [1.3, 99.0] {   // d'abord en respectant les parts de chaque session, puis sans
        for &(_, sid, t) in &cands {
            if length >= target_s {
                break;
            }
            let (a, b) = window(sessions[sid], t);
            let free = existing.get(sid).unwrap_or(&empty).iter().chain(&chosen[sid])
                .all(|c| !(a < c.end + 5.0 && b > c.start - 5.0));
            if used[sid] >= quota[sid] * relax || !free {
                continue;
            }
            chosen.get_mut(sid).unwrap().push(auto_clip(a, b));
            *used.get_mut(sid).unwrap() += b - a;
            length += (b - a) - if length != 0.0 { transition_s } else { 0.0 };
        }
    }
    chosen.into_iter()
        .filter(|(_, c)| !c.is_empty())
        .map(|(sid, mut c)| {
            c.sort_by(|x, y| x.start.total_cmp(&y.start));
            (sid.clone(), c)
        })
        .collect()
}
