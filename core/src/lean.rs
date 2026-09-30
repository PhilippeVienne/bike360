//! Angle d'inclinaison de la moto, déduit de l'horizon mesuré dans l'image.
//!
//! La caméra est fixée au guidon : elle s'incline avec la moto. Le roulis résiduel de
//! l'horizon (par rapport à l'inclinaison fixe du support) est donc l'angle de la moto, au
//! signe près (positif = penché à droite). À basse vitesse l'estimation est lâche (a priori
//! large, guidon braqué) : les statistiques ne comptent que les passages roulants.

use serde::{Deserialize, Serialize};

use crate::analyze::Analysis;
use crate::geometry::{self, Tilt};
use crate::horizon::HorizonData;
use crate::numeric::{gauss, interp, round_nd};

/// Limite de mesure : bord de la grille de roulis de l'horizon (un pic à cette valeur est plafonné,
/// à afficher « ≥ 44° »).
pub const LIMIT_DEG: f64 = 44.0;

/// Vitesse minimale (km/h) pour retenir l'angle dans les statistiques.
pub const MIN_SPEED_KMH: f64 = 25.0;

/// Angle (°) à chaque échantillon de l'horizon (HZ par seconde), positif = à droite.
pub fn lean_series(h: &HorizonData, tilt: &Tilt) -> Vec<f64> {
    let base_t = geometry::transpose(&geometry::tilt_matrix(Some(tilt)));
    h.up.iter()
        .map(|u| {
            let r = geometry::apply(&base_t, *u); // haut réel dans le repère redressé du support
            -((-r[0]).atan2(r[1]).to_degrees())
        })
        .collect()
}

/// Angle à l'instant t (temps de session), interpolé.
pub fn lean_at(series: &[f64], h: &HorizonData, t: f64) -> f64 {
    if series.is_empty() {
        return 0.0;
    }
    let x = ((t - h.t0.unwrap_or(0.0)) * h.hz as f64).clamp(0.0, (series.len() - 1) as f64);
    let i = x.floor() as usize;
    let j = (i + 1).min(series.len() - 1);
    series[i] + (series[j] - series[i]) * (x - i as f64)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeanStats {
    /// Angles maximaux (°, lissés sur ~1 s) à gauche et à droite, en roulant.
    pub max_left_deg: f64,
    pub max_right_deg: f64,
    /// Instants (s de session) correspondants.
    pub t_left: f64,
    pub t_right: f64,
    /// Un des maxima atteint la limite de mesure (valeur réelle peut-être supérieure).
    pub at_limit: bool,
}

/// Angles maximaux d'une session : lissage (pics d'une image ignorés), vitesse ≥ MIN_SPEED_KMH.
pub fn lean_stats(h: &HorizonData, result: &Analysis) -> Option<LeanStats> {
    let raw = lean_series(h, &result.tilt);
    if raw.len() < h.hz * 2 {
        return None;
    }
    let smooth = gauss(&raw, h.hz as f64 / 2.0);
    let speed: Vec<f64> = result.series.speed.iter().map(|v| v.unwrap_or(f64::NAN)).collect();
    let secs: Vec<f64> = (0..speed.len()).map(|k| k as f64).collect();
    let (known_t, known_v): (Vec<f64>, Vec<f64>) =
        secs.iter().zip(&speed).filter(|(_, v)| v.is_finite()).map(|(t, v)| (*t, *v)).unzip();
    if known_t.len() < 2 {
        return None;
    }
    let (mut best_l, mut best_r) = ((0.0, 0.0), (0.0, 0.0));
    for (k, a) in smooth.iter().enumerate() {
        let t = h.t0.unwrap_or(0.0) + k as f64 / h.hz as f64;
        if interp(t, &known_t, &known_v) < MIN_SPEED_KMH {
            continue;
        }
        if *a > best_r.0 {
            best_r = (*a, t);
        }
        if -a > best_l.0 {
            best_l = (-a, t);
        }
    }
    Some(LeanStats {
        max_left_deg: round_nd(best_l.0, 1),
        max_right_deg: round_nd(best_r.0, 1),
        t_left: round_nd(best_l.1, 1),
        t_right: round_nd(best_r.1, 1),
        at_limit: best_l.0.max(best_r.0) >= LIMIT_DEG - 0.5,
    })
}

/// Angle lissé prêt à incruster (jauge de la télémétrie).
#[derive(Debug, Clone)]
pub struct LeanTrack {
    series: Vec<f64>,
    t0: f64,
    hz: f64,
}

impl LeanTrack {
    /// Lissage d'environ ¼ s : la jauge ne tremble pas d'une image à l'autre.
    pub fn new(h: &HorizonData, tilt: &Tilt) -> Self {
        LeanTrack { series: gauss(&lean_series(h, tilt), h.hz as f64 / 4.0), t0: h.t0.unwrap_or(0.0), hz: h.hz as f64 }
    }

    /// Angle (°, positif = à droite) à l'instant t (temps de session).
    pub fn at(&self, t: f64) -> f64 {
        if self.series.is_empty() {
            return 0.0;
        }
        let x = ((t - self.t0) * self.hz).clamp(0.0, (self.series.len() - 1) as f64);
        let i = x.floor() as usize;
        let j = (i + 1).min(self.series.len() - 1);
        self.series[i] + (self.series[j] - self.series[i]) * (x - i as f64)
    }
}
