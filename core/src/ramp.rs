//! Accélérés d'un clip : points de vitesse posés dans l'éditeur (×1, ×2, ×4…), comme les
//! points clés de cadrage. Pas de ralenti (sources à 29,97 i/s : il faudrait inventer des
//! images) : la vitesse est au moins ×1.
//!
//! Entre deux points, la vitesse varie en douceur (courbe « douce », en échelle log : ×1 → ×4
//! passe par ×2 à mi-chemin) ; avant le premier et après le dernier point, elle est constante.
//! Le son d'origine n'est gardé que là où la vitesse est normale (fondu ailleurs).
//!
//! Stockage : champ `speed_keys` du clip, [{"t": s depuis le début du clip, "speed": facteur}].

use serde::{Deserialize, Serialize};

use crate::geometry::{ease, Clip};

pub const MAX_SPEED: f64 = 16.0;
/// Tolérance autour de ×1 pour garder le son d'origine.
const NORMAL_TOL: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SpeedKey {
    pub t: f64,
    pub speed: f64,
}

/// Points de vitesse d'un clip, triés et bornés (×1 à ×16) ; vide = vitesse normale.
pub fn speed_keys(clip: &Clip) -> Vec<SpeedKey> {
    let mut keys: Vec<SpeedKey> = clip.extra.get("speed_keys")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    keys.retain(|k| k.t.is_finite() && k.speed.is_finite());
    for k in &mut keys {
        k.speed = k.speed.clamp(1.0, MAX_SPEED);
    }
    keys.sort_by(|a, b| a.t.total_cmp(&b.t));
    keys
}

/// Vitesse à `t` secondes (temps source) depuis le début du clip.
pub fn speed_at(keys: &[SpeedKey], t: f64) -> f64 {
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return 1.0 };
    if t <= first.t {
        return first.speed;
    }
    if t >= last.t {
        return last.speed;
    }
    let i = keys.iter().position(|k| k.t > t).unwrap();
    let (a, b) = (keys[i - 1], keys[i]);
    let u = ease("ease_in_out", (t - a.t) / (b.t - a.t));
    (a.speed.ln() + (b.speed.ln() - a.speed.ln()) * u).exp()
}

/// Instants source (s depuis le début du clip) de chaque image de sortie à `fps`, pour un clip
/// de `length` secondes. Sans point de vitesse : une image source par image de sortie.
pub fn source_times(keys: &[SpeedKey], length: f64, fps: f64) -> Vec<f64> {
    let dt = 1.0 / fps;
    let mut out = vec![];
    let mut t = 0.0;
    while t < length - 1e-9 {
        out.push(t);
        // point milieu : pas d'avance fidèle même pendant les rampes
        let mid = t + speed_at(keys, t) * dt / 2.0;
        t += speed_at(keys, mid) * dt;
    }
    out
}

/// Durée de sortie (s) d'un clip de `length` secondes.
pub fn output_duration(keys: &[SpeedKey], length: f64, fps: f64) -> f64 {
    source_times(keys, length, fps).len() as f64 / fps
}

/// Passage à vitesse normale : son d'origine gardé. Temps de sortie [out_start, out_end),
/// début correspondant dans la source (s depuis le début du clip).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AudioSpan {
    pub out_start: f64,
    pub out_end: f64,
    pub src_start: f64,
}

/// Passages à vitesse normale (au moins `min_s` secondes), dans l'ordre.
pub fn audio_spans(keys: &[SpeedKey], length: f64, fps: f64, min_s: f64) -> Vec<AudioSpan> {
    let times = source_times(keys, length, fps);
    let mut out = vec![];
    let mut start: Option<usize> = None;
    for k in 0..=times.len() {
        let normal = k < times.len() && (speed_at(keys, times[k]) - 1.0).abs() <= NORMAL_TOL;
        match (normal, start) {
            (true, None) => start = Some(k),
            (false, Some(s)) => {
                if (k - s) as f64 / fps >= min_s {
                    out.push(AudioSpan { out_start: s as f64 / fps, out_end: k as f64 / fps, src_start: times[s] });
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 30000.0 / 1001.0;

    fn keys(v: &[(f64, f64)]) -> Vec<SpeedKey> {
        v.iter().map(|&(t, speed)| SpeedKey { t, speed }).collect()
    }

    #[test]
    fn sans_point_vitesse_normale() {
        let t = source_times(&[], 10.0, FPS);
        assert_eq!(t.len(), (10.0 * FPS).ceil() as usize);
        assert!((t[100] - 100.0 / FPS).abs() < 1e-9);
        assert_eq!(audio_spans(&[], 10.0, FPS, 0.5).len(), 1);
    }

    #[test]
    fn accelere_au_milieu() {
        // ×1 jusqu'à 5 s, rampe vers ×4 à 6 s, ×4 jusqu'à 26 s, retour à ×1 à 27 s, clip de 35 s
        let k = keys(&[(5.0, 1.0), (6.0, 4.0), (26.0, 4.0), (27.0, 1.0)]);
        assert!((speed_at(&k, 5.5) - 2.0).abs() < 1e-9);   // mi-rampe en échelle log
        let d = output_duration(&k, 35.0, FPS);
        // 5 + 20/4 + 8 = 18 s, plus deux rampes : ∫ dt / vitesse sur 1 s de source chacune
        let ramp: f64 = (0..10000).map(|i| 1e-4 / speed_at(&k, 5.0 + (i as f64 + 0.5) * 1e-4)).sum();
        assert!((d - (18.0 + 2.0 * ramp)).abs() < 1.5 / FPS, "durée {d}");
        let t = source_times(&k, 35.0, FPS);
        assert!(t.windows(2).all(|w| w[1] > w[0]), "instants croissants");
        let spans = audio_spans(&k, 35.0, FPS, 0.5);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].out_start == 0.0 && (spans[0].out_end - 5.0).abs() < 0.1);
        assert!((spans[1].src_start - 27.0).abs() < 0.1);
    }

    #[test]
    fn bornes() {
        let c: Clip = serde_json::from_value(serde_json::json!(
            {"start": 0, "end": 10, "speed_keys": [{"t": 3, "speed": 0.5}, {"t": 1, "speed": 99}]})).unwrap();
        assert_eq!(speed_keys(&c), keys(&[(1.0, MAX_SPEED), (3.0, 1.0)]));
    }
}
