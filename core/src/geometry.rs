//! Géométrie de vue : rotations, redressement de l'horizon, points clés des clips.
//!
//! Mêmes conventions que la visionneuse WebGL (ui/viewer.js, ui/geometry.js) et le moteur (reproject.cu) :
//! repère caméra x droite, y haut, z devant ; vue = rotation écran → caméra.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Mat3 = [[f64; 3]; 3];

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut o = [[0.0; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            o[r][c] = (0..3).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    o
}

pub fn transpose(a: &Mat3) -> Mat3 {
    let mut o = [[0.0; 3]; 3];
    for r in 0..3 {
        for c in 0..3 {
            o[r][c] = a[c][r];
        }
    }
    o
}

pub fn apply(a: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|r| a[r][0] * v[0] + a[r][1] * v[1] + a[r][2] * v[2])
}

/// Rotation élémentaire : 'p' (tangage, autour de x), 'y' (lacet, y), 'r' (roulis, z) ; degrés.
pub fn rot(axis: char, deg: f64) -> Mat3 {
    let (s, c) = deg.to_radians().sin_cos();
    match axis {
        'p' => [[1.0, 0.0, 0.0], [0.0, c, s], [0.0, -s, c]],
        'y' => [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]],
        _ => [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]],
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Tilt {
    pub pitch: f64,
    pub roll: f64,
}

/// Inclinaison fixe de la caméra (déduite de la gravité moyenne), cf. analyze::mount_tilt.
pub fn tilt_matrix(tilt: Option<&Tilt>) -> Mat3 {
    let t = tilt.copied().unwrap_or_default();
    mul(&rot('p', t.pitch), &rot('r', t.roll))
}

/// Rotation minimale amenant l'axe y (haut de la vue) sur `up` (haut réel, repère caméra).
pub fn min_rotation(up: [f64; 3]) -> Mat3 {
    let n = (up[0] * up[0] + up[1] * up[1] + up[2] * up[2]).sqrt();
    let u = up.map(|x| x / n);
    let axis = [-u[2], 0.0, u[0]]; // y × u
    let s = (axis[0] * axis[0] + axis[2] * axis[2]).sqrt();
    let c = u[1];
    if s < 1e-9 {
        return IDENTITY;
    }
    let k = axis.map(|x| x / s);
    let kk: Mat3 = [[0.0, -k[2], k[1]], [k[2], 0.0, -k[0]], [-k[1], k[0], 0.0]];
    let k2 = mul(&kk, &kk);
    let mut o = IDENTITY;
    for r in 0..3 {
        for col in 0..3 {
            o[r][col] += s * kk[r][col] + (1.0 - c) * k2[r][col];
        }
    }
    o
}

/// Rotation écran → caméra ; `level` = matrice de redressement (None = aucun).
/// `roll` : rotation manuelle de l'image autour de l'axe de visée (°, positif = sens horaire).
pub fn view_matrix(yaw: f64, pitch: f64, level: Option<&Mat3>, roll: f64) -> Mat3 {
    let m = mul(&mul(&rot('y', yaw), &rot('p', pitch)), &rot('r', roll));
    match level {
        Some(l) => mul(l, &m),
        None => m,
    }
}

/// Décompose une rotation en angles ffmpeg v360 : v360 applique Ry(y)·Rp(p)·Rr(−roll).
pub fn v360_angles(m: &Mat3) -> (f64, f64, f64) {
    let p2 = m[1][2].clamp(-1.0, 1.0).asin().to_degrees();
    let y2 = m[0][2].atan2(m[2][2]).to_degrees();
    let n = mul(&transpose(&mul(&rot('y', y2), &rot('p', p2))), m);
    (y2, p2, -(n[1][0].atan2(n[0][0])).to_degrees())
}

// ---------------------------------------------------------------- clips et points clés

fn default_fov() -> f64 {
    100.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub t: f64,
    #[serde(default)]
    pub yaw: f64,
    #[serde(default)]
    pub pitch: f64,
    #[serde(default)]
    pub roll: f64,
    #[serde(default = "default_fov")]
    pub fov: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve: Option<String>,
}

/// Clip tel qu'enregistré par l'interface (champs inconnus conservés tels quels).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub yaw: f64,
    #[serde(default)]
    pub pitch: f64,
    #[serde(default)]
    pub roll: f64,
    #[serde(default = "default_fov")]
    pub fov: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyframes: Option<Vec<Keyframe>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roll_keys: Option<Vec<(f64, f64)>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HorizonMode {
    Auto,
    Fixe,
    Aucun,
}

/// Mode d'horizon d'un clip : 'auto' (image), 'fixe' (support) ou 'aucun'.
/// Compatibilité : les anciens clips n'avaient qu'un booléen `level` (vrai → 'auto').
pub fn clip_horizon_mode(clip: &Clip) -> HorizonMode {
    match clip.horizon.as_deref() {
        Some("auto") => HorizonMode::Auto,
        Some("fixe") => HorizonMode::Fixe,
        Some("aucun") => HorizonMode::Aucun,
        _ if clip.level == Some(true) => HorizonMode::Auto,
        _ => HorizonMode::Aucun,
    }
}

/// Courbes de transition entre deux points clés (mêmes noms et formules que ui/geometry.js).
/// La courbe d'un point clé s'applique au segment qui le suit (comme l'app Insta360).
pub fn ease(curve: &str, u: f64) -> f64 {
    match curve {
        "ease_in_out" => u * u * (3.0 - 2.0 * u),
        "ease_in" => u * u,
        "ease_out" => 1.0 - (1.0 - u) * (1.0 - u),
        "quick" => u.powi(3) * (u * (6.0 * u - 15.0) + 10.0),
        "delay" => {
            if u < 0.5 {
                0.0
            } else {
                ((u - 0.5) * 2.0).powi(2) * (3.0 - 4.0 * (u - 0.5))
            }
        }
        "cut" => {
            if u < 1.0 {
                0.0
            } else {
                1.0
            }
        }
        _ => u,
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct View {
    pub yaw: f64,
    pub pitch: f64,
    pub roll: f64,
    pub fov: f64,
}

fn base_view(clip: &Clip) -> View {
    View { yaw: clip.yaw, pitch: clip.pitch, roll: clip.roll, fov: clip.fov }
}

/// Points clés d'un clip, triés ; compatibilité avec les anciens `roll_keys`.
pub fn clip_keyframes(clip: &Clip) -> Vec<Keyframe> {
    if let Some(k) = clip.keyframes.as_ref().filter(|k| !k.is_empty()) {
        let mut k = k.clone();
        k.sort_by(|a, b| a.t.total_cmp(&b.t));
        return k;
    }
    if let Some(r) = clip.roll_keys.as_ref().filter(|r| !r.is_empty()) {
        let b = base_view(clip);
        let mut r = r.clone();
        r.sort_by(|a, b| a.0.total_cmp(&b.0));
        return r.into_iter()
            .map(|(t, roll)| Keyframe { t, yaw: b.yaw, pitch: b.pitch, roll, fov: b.fov, curve: Some("linear".into()) })
            .collect();
    }
    vec![]
}

fn key_view(k: &Keyframe) -> View {
    View { yaw: k.yaw, pitch: k.pitch, roll: k.roll, fov: k.fov }
}

/// Cadrage (yaw, pitch, roll, fov en °) à `t_rel` secondes du début du clip.
pub fn clip_view_at(clip: &Clip, t_rel: f64) -> View {
    let keys = clip_keyframes(clip);
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else { return base_view(clip) };
    if t_rel <= first.t {
        return key_view(first);
    }
    if t_rel >= last.t {
        return key_view(last);
    }
    let i = keys.iter().position(|k| k.t > t_rel).unwrap();
    let (a, b) = (&keys[i - 1], &keys[i]);
    let u = ease(a.curve.as_deref().unwrap_or("linear"), (t_rel - a.t) / (b.t - a.t));
    let dyaw = (b.yaw - a.yaw + 180.0).rem_euclid(360.0) - 180.0; // plus court chemin
    View {
        yaw: (a.yaw + dyaw * u + 180.0).rem_euclid(360.0) - 180.0,
        pitch: a.pitch + (b.pitch - a.pitch) * u,
        roll: a.roll + (b.roll - a.roll) * u,
        fov: a.fov + (b.fov - a.fov) * u,
    }
}
