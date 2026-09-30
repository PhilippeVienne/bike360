//! Résumé hyperlapse d'une session : toute la balade en quelques minutes, vitesse variable.
//!
//! Le temps de sortie est réparti selon le score d'intérêt (analyze) : les moments forts sont
//! lents (quelques ×), les routes calmes très accélérées, les arrêts quasiment sautés. La
//! densité est lissée (pas d'à-coups) puis normalisée pour tenir la durée cible.

use serde::Serialize;

use crate::analyze::Analysis;
use crate::numeric::{gauss, interp, round_nd};

pub const OUT_FPS: f64 = 30000.0 / 1001.0;
const MAX_DENSITY: f64 = 0.25; // au plus lent : 4× (seconde de sortie par seconde de source)
const MIN_DENSITY: f64 = 1.0 / 400.0; // arrêts : ~400×
const SMOOTH_S: f64 = 6.0; // lissage de la vitesse de lecture

fn series(s: &[Option<f64>], default: f64) -> Vec<f64> {
    s.iter().map(|v| v.filter(|x| !x.is_nan()).unwrap_or(default)).collect()
}

/// Secondes de sortie par seconde de source, pour chaque seconde de la session.
pub fn density(result: &Analysis, target_s: f64) -> Vec<f64> {
    let score = series(&result.series.score, 0.0);
    let speed = series(&result.series.speed, 30.0);
    let raw: Vec<f64> = score.iter().zip(&speed)
        .map(|(s, v)| if *v < 5.0 { MIN_DENSITY } else { 0.01 + 0.99 * s.clamp(0.0, 1.0).powi(2) })
        .collect();
    // lissage en échelle log (facteurs)
    let raw: Vec<f64> = gauss(&raw.iter().map(|v| v.ln()).collect::<Vec<_>>(), SMOOTH_S).iter().map(|v| v.exp()).collect();
    let total: f64 = raw.iter().sum();
    let mut d: Vec<f64> = raw.iter().map(|v| v * target_s / total).collect();
    for _ in 0..20 {   // bornes puis renormalisation
        for v in d.iter_mut() {
            *v = v.clamp(MIN_DENSITY, MAX_DENSITY);
        }
        let free: Vec<bool> = d.iter().map(|v| *v > MIN_DENSITY && *v < MAX_DENSITY).collect();
        let excess = target_s - d.iter().sum::<f64>();
        let free_sum: f64 = d.iter().zip(&free).filter(|(_, f)| **f).map(|(v, _)| v).sum();
        if excess.abs() < 0.01 || !free.iter().any(|f| *f) {
            break;
        }
        for (v, f) in d.iter_mut().zip(&free) {
            if *f {
                *v *= 1.0 + excess / free_sum;
            }
        }
    }
    d
}

/// Instants de session (s) des images de sortie, à OUT_FPS.
pub fn frame_times(result: &Analysis, target_s: f64) -> Vec<f64> {
    let d = density(result, target_s);
    let mut cum = vec![0.0]; // temps de sortie à chaque seconde de source
    for v in &d {
        cum.push(cum.last().unwrap() + v);
    }
    let idx: Vec<f64> = (0..cum.len()).map(|k| k as f64).collect();
    let n_out = (cum.last().unwrap() * OUT_FPS) as usize;
    (0..n_out).map(|k| interp(k as f64 / OUT_FPS, &cum, &idx)).collect()
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub output_s: f64,
    pub fastest_x: i64,
    pub slowest_x: f64,
}

pub fn summary(result: &Analysis, target_s: f64) -> Summary {
    let d = density(result, target_s);
    let (lo, hi) = d.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(*v), b.max(*v)));
    Summary {
        output_s: round_nd(d.iter().sum(), 1),
        fastest_x: (1.0 / lo).round_ties_even() as i64,
        slowest_x: round_nd(1.0 / hi, 1),
    }
}
