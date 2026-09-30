//! Confidentialité : détection des visages et plaques, suivi, floutage à l'export.
//!
//! Analyse : chaque clip est rendu (moteur GPU) dans son cadrage en 1920×1080 ; on y détecte
//! visages (YuNet, image entière) et plaques (YOLOv9, tuiles 2×2 : les plaques de moto sont
//! petites), puis on relie les détections d'image en image en pistes. Chaque piste est stockée
//! en directions dans le repère caméra (sphère) et en demi-angles : le floutage suit donc
//! quel que soit le cadrage, le format de sortie ou le temps (hyperlapse).
//!
//! Export : les pistes actives sont reprojetées dans chaque image de sortie (`frame_boxes`,
//! floutées par le moteur GPU) ; `blur_video` est le chemin de repli (ffmpeg).
//!
//! Mêmes fichiers que la version Python (data/privacy/<session>.json, vignettes dans
//! data/cache/privacy) : les deux versions coexistent.

pub mod imgproc;
pub mod nets;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::geometry::{self, Mat3};
use crate::numeric::{round_nd, searchsorted};
use crate::paths;
pub use imgproc::Image;
use imgproc::{components, gaussian_blur, light_mask, match_template_best, resize_area, resize_linear};
use nets::{RfDetr, VitNet, VitTracker, Yolo, YuNet, COCO_MOTORCYCLE};

pub const AW: usize = 1920; // rendu d'analyse (cadrage et champ du clip)
pub const AH: usize = 1080;
pub const DETECT_EVERY: usize = 2; // une image sur deux : le suivi image par image comble (à 3, et
pub const TILES_EVERY: usize = 1; // tuiles une fois sur deux, ~40 fuites de plus sur le montage test)
pub const FACE_SCORE: f32 = 0.6;
pub const PLATE_SCORE: f32 = 0.35;
pub const FACE_SCALE: f64 = 0.5; // détection des visages à mi-résolution (≈ 4× plus rapide)
pub const MOTO_SCORE: f32 = 0.35;
pub const MOTO_MIN_H: i32 = 45; // moto plus petite (px, image 1080p) : plaque illisible, ignorée
pub const TRACK_GAP: usize = 12; // images sans détection tolérées dans une piste
pub const MIN_HITS: usize = 2; // piste retenue si ≥ 2 détections, ou une seule très sûre
pub const SURE_CONF: f64 = 0.75;
pub const EXTEND_S: f64 = 0.25; // floutage prolongé avant/après la piste
pub const PAD: f64 = 0.3; // marge autour de la zone détectée

pub fn data_dir() -> PathBuf {
    paths::data().join("privacy")
}

/// Vignettes de revue (data/cache/privacy).
pub fn thumbs_dir() -> PathBuf {
    paths::cache().join("privacy")
}

/// Boîte écran (x, y, largeur, hauteur) en pixels.
pub type BoxF = [f64; 4];

/// Détection d'une image : type ("visage" ou "plaque"), confiance, boîte.
#[derive(Clone, Debug, Serialize)]
pub struct Detection {
    pub kind: &'static str,
    pub conf: f64,
    pub bbox: BoxF,
}

// ------------------------------------------------------------------ détecteur

/// Visages (YuNet) + plaques (YOLOv9 en tuiles) + plaques de moto (RF-DETR), sur images BGR.
/// Contient aussi le réseau de suivi (VitTrack) utilisé par `analyze_clip`.
pub struct Detector {
    face: YuNet,
    plates: Yolo,
    vehicles: RfDetr,
    pub tracker: VitNet,
}

// Le détecteur et le réseau de suivi peuvent être déplacés dans un fil de travail (serveur).
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Detector>();
    send::<VitNet>();
};

impl Detector {
    /// Modèles chargés sur la carte graphique si ONNX Runtime CUDA est utilisable (sinon processeur).
    pub fn new() -> Result<Self> {
        Self::with_gpu(true)
    }

    pub fn with_gpu(gpu: bool) -> Result<Self> {
        Ok(Detector {
            face: YuNet::new(&nets::project_model(nets::FACE_MODEL)?, gpu, FACE_SCORE, 0.3, 5000)?,
            plates: Yolo::new(&nets::oim_model(nets::PLATE_MODEL)?, gpu, PLATE_SCORE)?,
            vehicles: RfDetr::new(&nets::oim_model(nets::VEHICLE_MODEL)?, gpu, MOTO_SCORE)?,
            // suiveur sur processeur (comme OpenCV) : petit réseau appelé à chaque image, plus
            // rapide sans aller-retour vers la carte graphique (2,2 ms contre 6 ms par mise à jour)
            tracker: VitNet::new(&nets::project_model(nets::TRACK_MODEL)?, false)?,
        })
    }

    /// Détections d'une image : visages, plaques (image entière + tuiles), plaques de moto.
    pub fn detect(&mut self, img: &Image, tiles: bool) -> Result<Vec<Detection>> {
        let (w, h) = (img.w, img.h);
        let mut out = vec![];
        // visages à mi-résolution : un visage trop petit pour y être vu n'est pas reconnaissable
        let small = resize_area(img, (w as f64 * FACE_SCALE) as usize, (h as f64 * FACE_SCALE) as usize);
        for f in self.face.detect(&small)? {
            out.push(Detection {
                kind: "visage",
                conf: f[4] as f64,
                bbox: [0, 1, 2, 3].map(|k| f[k] as f64 / FACE_SCALE),
            });
        }
        // image entière (plaques proches, qui chevauchent deux tuiles) + tuiles 2×2 avec
        // recouvrement (plaques de moto lointaines : ~2 % de la largeur)
        let ov = 100usize;
        let mut views = vec![(0usize, 0usize)];
        if tiles {
            for ty in [0, h / 2 - ov] {
                for tx in [0, w / 2 - ov] {
                    views.push((tx, ty));
                }
            }
        }
        let mut boxes: Vec<[f64; 5]> = vec![];
        for (i, &(tx, ty)) in views.iter().enumerate() {
            let tile;
            let view = if i == 0 {
                img
            } else {
                tile = img.crop(tx as i64, ty as i64, (tx + w / 2 + ov) as i64, (ty + h / 2 + ov) as i64);
                &tile
            };
            for d in self.plates.detect(view)? {
                boxes.push([(d.x1 as i64 + tx as i64) as f64, (d.y1 as i64 + ty as i64) as f64, d.w() as f64,
                            d.h() as f64, d.conf as f64]);
            }
        }
        // fusion par union : une plaque vue en deux moitiés (bord de tuile) est floutée en entier
        boxes.sort_by(|a, b| b[4].total_cmp(&a[4]));
        let mut merged: Vec<[f64; 5]> = vec![];
        for [x, y, bw, bh, c] in boxes {
            let mut hit = false;
            for m in merged.iter_mut() {
                let ix = ((x + bw).min(m[0] + m[2]) - x.max(m[0])).max(0.0);
                let iy = ((y + bh).min(m[1] + m[3]) - y.max(m[1])).max(0.0);
                if ix * iy > 0.2 * (bw * bh).min(m[2] * m[3]) {
                    let (x0, y0) = (x.min(m[0]), y.min(m[1]));
                    m[2] = (x + bw).max(m[0] + m[2]) - x0;
                    m[3] = (y + bh).max(m[1] + m[3]) - y0;
                    m[0] = x0;
                    m[1] = y0;
                    m[4] = c.max(m[4]);
                    hit = true;
                    break;
                }
            }
            if !hit {
                merged.push([x, y, bw, bh, c]);
            }
        }
        for m in &merged {
            out.push(Detection { kind: "plaque", conf: m[4], bbox: [m[0], m[1], m[2], m[3]] });
        }
        out.extend(self.moto_plates(img, &merged)?);
        Ok(out)
    }

    /// Plaques de moto : le modèle de plaques (appris sur des voitures) ne les reconnaît pas.
    ///
    /// On trouve la moto (détecteur de véhicules), puis sur elle le rectangle clair de la
    /// plaque (bande centrale) ; à défaut, si la moto est proche, la zone habituelle de la
    /// plaque arrière est floutée par prudence (une moto vue de face n'en a pas : tache
    /// sans conséquence).
    fn moto_plates(&mut self, img: &Image, known: &[[f64; 5]]) -> Result<Vec<Detection>> {
        let mut out = vec![];
        let ih = img.h as f64;
        for d in self.vehicles.detect(img)? {
            if d.class != COCO_MOTORCYCLE {
                continue;
            }
            let (x, y, w, h) = (d.x1, d.y1, d.w(), d.h());
            if h < MOTO_MIN_H || h as f64 > ih * 0.6 || (y + h) as f64 > ih * 0.95 {
                continue; // trop loin, ou notre propre moto (en bas de l'image)
            }
            let (xf, yf, wf, hf) = (x as f64, y as f64, w as f64, h as f64);
            if known.iter().any(|k| k[0] < xf + wf && k[0] + k[2] > xf && k[1] < yf + hf && k[1] + k[3] > yf) {
                continue; // plaque déjà trouvée par le modèle de plaques
            }
            let crop = img.crop(x.max(0) as i64, y.max(0) as i64, (x + w) as i64, (y + h) as i64);
            if crop.is_empty() {
                continue;
            }
            let mut mask = light_mask(&crop);
            // bande centrale
            let (r0, r1) = ((hf * 0.2) as usize, (hf * 0.85) as usize);
            let (c0, c1) = ((wf * 0.2) as usize, (wf * 0.8) as usize);
            for r in 0..crop.h {
                for c in 0..crop.w {
                    if !(r >= r0 && r < r1 && c >= c0 && c < c1) {
                        mask[r * crop.w + c] = false;
                    }
                }
            }
            let mut best: Option<imgproc::Component> = None;
            for s in components(&mask, crop.w, crop.h) {
                let (bw, bh, area) = (s.w as f64, s.h as f64, s.area as f64);
                let ratio = bw / bh.max(1.0);
                if 0.02 * wf * hf <= area && area <= 0.2 * wf * hf && (0.7..=2.5).contains(&ratio) && area >= 0.6 * bw * bh
                    && best.is_none_or(|b| s.area > b.area)
                {
                    best = Some(s);
                }
            }
            if let Some(b) = best {
                out.push(Detection { kind: "plaque", conf: 0.5,
                                     bbox: [xf + b.x as f64, yf + b.y as f64, b.w as f64, b.h as f64] });
            } else if hf >= 1.5 * MOTO_MIN_H as f64 && hf >= 0.9 * wf {
                // zone de secours seulement pour une moto vue de face ou de dos (plus haute que
                // large) : de profil, la plaque n'est pas lisible et on flouterait la moto entière
                out.push(Detection { kind: "plaque", conf: 0.35, bbox: [xf + 0.3 * wf, yf + 0.35 * hf, 0.4 * wf, 0.3 * hf] });
            }
        }
        Ok(out)
    }
}

pub fn iou(a: &BoxF, b: &BoxF) -> f64 {
    let ix = ((a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0])).max(0.0);
    let iy = ((a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1])).max(0.0);
    let inter = ix * iy;
    inter / (a[2] * a[3] + b[2] * b[3] - inter + 1e-9)
}

/// Piste écran (ancienne méthode sans suiveur, gardée pour les outils).
#[derive(Clone, Debug, Serialize)]
pub struct ScreenTrack {
    pub kind: &'static str,
    pub hits: Vec<(usize, f64, BoxF)>,
}

/// Relie les détections [(indice d'image, détections)] en pistes.
pub fn link(frames: &[(usize, Vec<Detection>)]) -> Vec<ScreenTrack> {
    let mut tracks: Vec<ScreenTrack> = vec![];
    let mut active: Vec<usize> = vec![];
    for (fi, dets) in frames {
        active.retain(|&t| fi - tracks[t].hits.last().unwrap().0 <= TRACK_GAP);
        let mut used: Vec<usize> = vec![];
        let mut sorted: Vec<&Detection> = dets.iter().collect();
        sorted.sort_by(|a, b| b.conf.total_cmp(&a.conf));
        for d in sorted {
            let b = d.bbox;
            let (mut best, mut score) = (None, 0.0);
            for &t in &active {
                if tracks[t].kind != d.kind || used.contains(&t) {
                    continue;
                }
                let last = tracks[t].hits.last().unwrap().2;
                // recouvrement, ou centre proche (objet rapide entre deux détections)
                let (cx, cy) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
                let (lx, ly) = (last[0] + last[2] / 2.0, last[1] + last[3] / 2.0);
                let near = (cx - lx).hypot(cy - ly) < 1.5 * b[2].max(b[3]).max(last[2]).max(last[3]);
                let s = iou(&b, &last) + if near { 0.1 } else { 0.0 };
                if s > score && (s > 0.15 || near) {
                    best = Some(t);
                    score = s;
                }
            }
            let t = best.unwrap_or_else(|| {
                tracks.push(ScreenTrack { kind: d.kind, hits: vec![] });
                active.push(tracks.len() - 1);
                tracks.len() - 1
            });
            tracks[t].hits.push((*fi, d.conf, b));
            used.push(t);
        }
    }
    tracks.into_iter()
        .filter(|t| t.hits.len() >= MIN_HITS || t.hits.iter().map(|h| h.1).fold(f64::MIN, f64::max) >= SURE_CONF)
        .collect()
}

// ------------------------------------------------------------------ géométrie écran ↔ sphère

fn dot(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn norm(a: &[f64; 3]) -> f64 {
    dot(a, a).sqrt()
}

/// Rayon (repère vue) du pixel (u, v) ; x droite, y haut, z devant.
fn ray(u: f64, v: f64, w: f64, h: f64, hfov: f64) -> [f64; 3] {
    let th = (hfov.to_radians() / 2.0).tan();
    let tv = th * h / w;
    [(2.0 * u / w - 1.0) * th, -(2.0 * v / h - 1.0) * tv, 1.0]
}

/// Boîte écran → (direction caméra unitaire, demi-angle horizontal, vertical) en radians.
pub fn box_to_sphere(b: &BoxF, m: &Mat3, hfov: f64, w: f64, h: f64) -> ([f64; 3], f64, f64) {
    let [x, y, bw, bh] = *b;
    let c = ray(x + bw / 2.0, y + bh / 2.0, w, h, hfov);
    let ex = ray(x + bw, y + bh / 2.0, w, h, hfov);
    let ey = ray(x + bw / 2.0, y + bh, w, h, hfov);
    let ang = |a: &[f64; 3], b: &[f64; 3]| (dot(a, b) / (norm(a) * norm(b))).clamp(-1.0, 1.0).acos();
    let nc = norm(&c);
    let d = geometry::apply(m, c.map(|v| v / nc));
    let nd = norm(&d);
    (d.map(|v| v / nd), ang(&c, &ex), ang(&c, &ey))
}

/// Direction caméra + demi-angles → boîte écran (x, y, w, h) avec marge, ou None si derrière.
pub fn sphere_to_box(d: &[f64; 3], ax: f64, ay: f64, m: &Mat3, hfov: f64, w: f64, h: f64) -> Option<BoxF> {
    let v = geometry::apply(&geometry::transpose(m), *d);
    if v[2] <= 0.05 {
        return None;
    }
    let th = (hfov.to_radians() / 2.0).tan();
    let tv = th * h / w;
    let u = (v[0] / v[2] / th + 1.0) * w / 2.0;
    let y = (1.0 - v[1] / v[2] / tv) * h / 2.0;
    // taille : demi-angle rapporté au champ au centre de la zone (perspective incluse)
    let r = (v[0] / v[2]).hypot(v[1] / v[2]);
    let stretch = 1.0 + r * r;
    let hw = ax.tan() * stretch / th * w / 2.0 * (1.0 + PAD);
    let hh = ay.tan() * stretch / tv * h / 2.0 * (1.0 + PAD);
    Some([u - hw, y - hh, 2.0 * hw, 2.0 * hh])
}

/// Échantillon sphère : [t session, dx, dy, dz, ax, ay].
pub type Sample = [f64; 6];

/// Positions écran [(image, boîte)] → échantillons sphère arrondis comme en Python.
pub fn to_samples(hits: &[(usize, BoxF)], times: &[f64], mats: &[Mat3], fovs: &[f64]) -> Vec<Sample> {
    hits.iter()
        .map(|(fi, b)| {
            let (d, ax, ay) = box_to_sphere(b, &mats[*fi], fovs[*fi], AW as f64, AH as f64);
            [round_nd(times[*fi], 3), round_nd(d[0], 5), round_nd(d[1], 5), round_nd(d[2], 5), round_nd(ax, 5),
             round_nd(ay, 5)]
        })
        .collect()
}

// ------------------------------------------------------------------ pistes (format des fichiers)

/// Piste enregistrée (détectée ou tracée à la main). Champs inconnus conservés.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub id: Value,
    pub kind: String,
    #[serde(default)]
    pub conf: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub samples: Vec<Sample>,
    /// confiance de la vignette retenue (interne à merge_fragments, jamais écrite)
    #[serde(skip)]
    pub thumb_conf: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Track {
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }
}

/// Entrée d'un clip : empreinte du cadrage analysé, pistes détectées, zones manuelles.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClipEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<Vec<Track>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manual: Option<Vec<Track>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Fichier d'une session : identifiant de clip → entrée.
pub type SessionData = BTreeMap<String, ClipEntry>;

pub const MERGE_GAP_S: f64 = 0.35; // recollage : objets rapides (voiture croisée de près)
pub const MERGE_ANGLE: f64 = 30.0;
pub const HOLD_GAP_S: f64 = 3.0; // … et objets suivis perdus quelques secondes (plaque en bord d'image,
pub const HOLD_ANGLE: f64 = 5.0; // cahots) : même direction → la zone est tenue entre les deux

fn angle_deg(a: &[f64], b: &[f64]) -> f64 {
    (a[0] * b[0] + a[1] * b[1] + a[2] * b[2]).clamp(-1.0, 1.0).acos().to_degrees()
}

/// Recolle sur la sphère les fragments d'un même objet (même type, qui se suivent de près).
///
/// Un véhicule croisé de près traverse l'image de plusieurs degrés par image : le suivi à
/// l'écran le coupe en morceaux, ce qui laisserait la plaque nette entre deux morceaux.
pub fn merge_fragments(mut tracks: Vec<Track>) -> Vec<Track> {
    tracks.sort_by(|a, b| a.samples[0][0].total_cmp(&b.samples[0][0]));
    let mut out: Vec<Track> = vec![];
    for t in tracks {
        let s0 = t.samples[0];
        let mut best: Option<(usize, f64)> = None;
        for (i, o) in out.iter().enumerate() {
            let e = o.samples.last().unwrap();
            let gap = s0[0] - e[0];
            if o.kind != t.kind || !(-0.15..=HOLD_GAP_S).contains(&gap) {
                continue;
            }
            let ang = angle_deg(&e[1..4], &s0[1..4]);
            let ok = (gap <= MERGE_GAP_S && ang <= MERGE_ANGLE) || (gap <= HOLD_GAP_S && ang <= HOLD_ANGLE);
            if ok && best.is_none_or(|b| ang < b.1) {
                best = Some((i, ang));
            }
        }
        let Some((bi, _)) = best else {
            out.push(t);
            continue;
        };
        // chevauchement : la piste plus récente fait foi (l'ancienne n'y a souvent plus qu'un suiveur
        // à la traîne) ; jamais deux positions au même instant
        let b = &mut out[bi];
        let mut samples: Vec<Sample> = b.samples.iter().filter(|x| x[0] < s0[0] - 1e-3).copied().collect();
        samples.extend(t.samples.iter().copied());
        samples.sort_by(|a, b| a[0].total_cmp(&b[0]));
        b.samples = samples;
        b.conf = b.conf.max(t.conf);
        if t.conf > b.thumb_conf {
            b.thumb = t.thumb.clone();
            b.thumb_conf = t.conf;
        }
    }
    for (k, t) in out.iter_mut().enumerate() {
        t.id = Value::from(k);
        t.thumb_conf = 0.0;
    }
    out
}

/// Pistes détectées + zones tracées à la main d'un clip.
pub fn all_tracks(entry: Option<&ClipEntry>) -> Vec<Track> {
    let Some(e) = entry else { return vec![] };
    e.tracks.iter().flatten().chain(e.manual.iter().flatten()).cloned().collect()
}

/// Zone active : direction caméra unitaire et demi-angles (radians).
pub type Region = ([f64; 3], f64, f64);

/// Zones actives à l'instant t (session), interpolées entre échantillons.
pub fn regions_at(tracks: &[Track], t: f64) -> Vec<Region> {
    let mut out = vec![];
    for tr in tracks {
        let s = &tr.samples;
        if !tr.is_enabled() || s.is_empty() || !(s[0][0] - EXTEND_S <= t && t <= s[s.len() - 1][0] + EXTEND_S) {
            continue;
        }
        let ts: Vec<f64> = s.iter().map(|x| x[0]).collect();
        let k = searchsorted(&ts, t);
        let (d, ax, ay);
        if k == 0 || k >= s.len() {
            let a = if k == 0 { &s[0] } else { &s[s.len() - 1] };
            d = [a[1], a[2], a[3]];
            (ax, ay) = (a[4], a[5]);
        } else {
            let (a, b) = (&s[k - 1], &s[k]);
            let f = (t - a[0]) / (b[0] - a[0]).max(1e-6);
            let gap = b[0] - a[0];
            if gap > TRACK_GAP as f64 / 15.0 {
                // trou : tenu seulement si l'objet est resté dans la même direction
                let ang = angle_deg(&a[1..4], &b[1..4]);
                if gap > HOLD_GAP_S || ang > HOLD_ANGLE {
                    continue;
                }
            }
            d = [0, 1, 2].map(|i| a[1 + i] * (1.0 - f) + b[1 + i] * f);
            (ax, ay) = (a[4] * (1.0 - f) + b[4] * f, a[5] * (1.0 - f) + b[5] * f);
        }
        let n = norm(&d);
        out.push((d.map(|v| v / n), ax, ay));
    }
    out
}

// ------------------------------------------------------------------ zones tracées à la main

pub const MANUAL_WINDOW_S: f64 = 15.0; // suivi jusqu'à 15 s avant et après l'instant du tracé
pub const MANUAL_SIZE: usize = 400; // vue locale carrée centrée sur la zone
pub const MANUAL_STEP: usize = 2; // une image sur deux
pub const MANUAL_GOOD_SCORE: f32 = 0.35; // position acceptée
pub const MANUAL_MIN_SCORE: f32 = 0.15; // en dessous : suivi perdu
pub const MANUAL_HOLD_S: f64 = 2.0; // zone tenue à sa dernière position sûre (passage devant, flou…)
pub const MANUAL_MATCH: f64 = 0.55; // corrélation avec l'image d'origine de la zone : élément retrouvé

/// Vue (écran → caméra) regardant dans la direction d.
pub fn local_view(d: &[f64; 3]) -> Mat3 {
    let n = norm(d);
    let d = d.map(|v| v / n);
    geometry::view_matrix(d[0].atan2(d[2]).to_degrees(), d[1].clamp(-1.0, 1.0).asin().to_degrees(), None, 0.0)
}

/// Champ de la vue locale : la zone en occupe ~1/5, entre 25 et 90°.
pub fn local_fov(ax: f64, ay: f64) -> f64 {
    ((2.0 * ax.max(ay)).to_degrees() * 5.0).clamp(25.0, 90.0)
}

/// Boîte sans marge (sphere_to_box en ajoute une) : initialisation du suivi.
pub fn tight_box(d: &[f64; 3], ax: f64, ay: f64, m: &Mat3, fov: f64, size: usize) -> Option<[i32; 4]> {
    let [x, y, w, h] = sphere_to_box(d, ax, ay, m, fov, size as f64, size as f64)?;
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (w, h) = (w / (1.0 + PAD), h / (1.0 + PAD));
    Some([(cx - w / 2.0) as i32, (cy - h / 2.0) as i32, (w as i32).max(4), (h as i32).max(4)])
}

/// Lecteur d'images BGR d'une vidéo (ffmpeg en sous-processus).
pub struct FrameReader {
    child: Child,
    out: ChildStdout,
    pub w: usize,
    pub h: usize,
}

impl FrameReader {
    pub fn open(path: &Path, w: usize, h: usize) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-f", "rawvideo", "-pix_fmt", "bgr24", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .context("ffmpeg introuvable")?;
        let out = child.stdout.take().context("ffmpeg : pas de sortie")?;
        Ok(FrameReader { child, out, w, h })
    }

    /// Image suivante, ou None en fin de flux.
    pub fn next_frame(&mut self) -> Option<Image> {
        let mut buf = vec![0u8; self.w * self.h * 3];
        self.out.read_exact(&mut buf).ok()?;
        Some(Image::from_bgr(self.w, self.h, buf))
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Images BGR d'un rendu carré `size`×`size` (une sur `step`).
pub fn decode(h264: &Path, size: usize, step: usize) -> Result<Vec<Image>> {
    let mut r = FrameReader::open(h264, size, size)?;
    let mut frames = vec![];
    let mut i = 0;
    while let Some(f) = r.next_frame() {
        if i % step == 0 {
            frames.push(f);
        }
        i += 1;
    }
    Ok(frames)
}

fn crop_box(img: &Image, b: [i32; 4]) -> Image {
    img.crop(b[0] as i64, b[1] as i64, (b[0] + b[2]) as i64, (b[1] + b[3]) as i64)
}

/// Suit `bbox` depuis l'image k0 vers l'avant (+1) ou l'arrière (−1) : {indice: boîte}.
///
/// Score faible (occultation, flou de mouvement) : la zone reste à sa dernière position sûre
/// au lieu de dériver avec le suiveur ; au-delà de MANUAL_HOLD_S sans position sûre, arrêt.
pub fn follow(net: &mut VitNet, frames: &[Image], k0: usize, bbox: [i32; 4], direction: i64, fps: f64)
    -> Result<BTreeMap<usize, [i32; 4]>> {
    let mut tracker = VitTracker::init(&frames[k0], bbox);
    let mut out = BTreeMap::from([(k0, bbox)]);
    let size = frames[k0].h as i32;
    let [_, _, w0, h0] = bbox;
    let template = crop_box(&frames[k0], bbox);
    let (mut last, mut weak) = (bbox, 0usize);
    let max_weak = (MANUAL_HOLD_S * fps / MANUAL_STEP as f64) as usize;
    // recherche de l'image d'origine de la zone autour de la dernière position sûre
    let rematch = |img: &Image, around: [i32; 4]| -> Option<[i32; 4]> {
        let [x, y, w, h] = around;
        let m = w.max(h); // fenêtre étroite : pendant une occultation l'élément bouge peu
        let (xa, ya) = ((x - m).max(0), (y - m).max(0));
        let area = img.crop(xa as i64, ya as i64, size.min(x + w + m) as i64, size.min(y + h + m) as i64);
        if area.h as i32 <= h0 || area.w as i32 <= w0 || template.is_empty() {
            return None;
        }
        let (best, lx, ly) = match_template_best(&area, &template)?;
        (best >= MANUAL_MATCH).then_some([xa + lx as i32, ya + ly as i32, w0, h0])
    };
    let mut k = k0 as i64 + direction;
    while k >= 0 && (k as usize) < frames.len() {
        let img = &frames[k as usize];
        let (ok, b) = tracker.update(net, img)?;
        let score = if ok { tracker.score } else { 0.0 };
        let [x, y, w, h] = b;
        let inside = !(x < 2 || y < 2 || x + w > size - 2 || y + h > size - 2);
        if ok && score >= MANUAL_GOOD_SCORE && inside {
            last = b;
            weak = 0;
        } else if let Some(found) = rematch(img, last) {
            // élément retrouvé (fin d'occultation) : on repart de là
            last = found;
            weak = 0;
            tracker = VitTracker::init(img, found);
        } else if weak < max_weak {
            weak += 1;
        } else {
            break;
        }
        out.insert(k as usize, last);
        k += direction;
    }
    Ok(out)
}

/// Zone fixe dans le repère caméra (élément solidaire de la moto) : un échantillon toutes les
/// 0,5 s sur [start, end + 0,5[ (comme server.run_manual_zone).
pub fn fixed_zone_samples(start: f64, end: f64, d0: &[f64; 3], ax: f64, ay: f64) -> Vec<Sample> {
    let n = norm(d0);
    let d = d0.map(|v| v / n);
    let count = ((end + 0.5 - start) / 0.5).ceil().max(0.0) as usize;
    (0..count)
        .map(|i| {
            let t = start + i as f64 * 0.5;
            [round_nd(t, 3), round_nd(d[0], 5), round_nd(d[1], 5), round_nd(d[2], 5), round_nd(ax, 5), round_nd(ay, 5)]
        })
        .collect()
}

/// Zone tracée à la main à l'instant t0, suivie dans la vue locale (`frames` rendues avec la
/// vue `m` et le champ `fov`, une image sur MANUAL_STEP, instants `times`) : échantillons sphère
/// et vignette (comme server.run_manual_zone).
#[allow(clippy::too_many_arguments)]
pub fn track_manual_zone(net: &mut VitNet, frames: &[Image], times: &[f64], fps: f64, t0: f64, d0: &[f64; 3], ax: f64,
                         ay: f64, m: &Mat3, fov: f64) -> Result<(Vec<Sample>, Option<Image>)> {
    if frames.is_empty() {
        bail!("aucune image rendue autour de la zone");
    }
    let size = frames[0].w;
    let mut k0 = 0;
    for (k, t) in times.iter().enumerate() {
        if (t - t0).abs() < (times[k0] - t0).abs() {
            k0 = k;
        }
    }
    let box0 = tight_box(d0, ax, ay, m, fov, size).context("zone derrière la vue locale")?;
    let mut boxes = follow(net, frames, k0, box0, -1, fps)?;
    boxes.extend(follow(net, frames, k0, box0, 1, fps)?);
    let s = size as f64;
    let samples = boxes.iter()
        .map(|(k, b)| {
            let (d, bx, by) = box_to_sphere(&b.map(|v| v as f64), m, fov, s, s);
            [round_nd(times[*k], 3), round_nd(d[0], 5), round_nd(d[1], 5), round_nd(d[2], 5), round_nd(bx, 5), round_nd(by, 5)]
        })
        .collect();
    let [x, y, w, h] = box0;
    let mm = w.max(h);
    let crop = frames[k0].crop((x - mm).max(0) as i64, (y - mm).max(0) as i64, (x + w + mm) as i64, (y + h + mm) as i64);
    let thumb = (!crop.is_empty())
        .then(|| resize_linear(&crop, 120, ((120 * crop.h) as f64 / crop.w as f64).max(1.0) as usize));
    Ok((samples, thumb))
}

// ------------------------------------------------------------------ fichiers

pub fn path(sid: &str) -> PathBuf {
    data_dir().join(format!("{sid}.json"))
}

pub fn load(sid: &str) -> Result<SessionData> {
    let p = path(sid);
    if !p.exists() {
        return Ok(SessionData::new());
    }
    serde_json::from_slice(&std::fs::read(&p)?).with_context(|| format!("lecture de {}", p.display()))
}

pub fn save(sid: &str, data: &SessionData) -> Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(path(sid), serde_json::to_vec(data)?)?;
    Ok(())
}

/// Flottant écrit comme `repr` de Python (json.dumps).
fn py_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let a = x.abs();
    if a == 0.0 || (1e-4..1e16).contains(&a) {
        let s = format!("{x}");
        return if s.contains('.') { s } else { s + ".0" };
    }
    let s = format!("{x:e}");
    let (m, e) = s.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
}

fn py_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{u:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// json.dumps(v, sort_keys=True) de Python, octet pour octet.
pub fn py_dumps(v: &Value) -> String {
    let mut out = String::new();
    py_dump(v, &mut out);
    out
}

fn py_dump(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => out.push_str(&i.to_string()),
            (_, Some(u)) => out.push_str(&u.to_string()),
            _ => out.push_str(&py_float(n.as_f64().unwrap_or(f64::NAN))),
        },
        Value::String(s) => py_str(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_dump(x, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_str(k, out);
                out.push_str(": ");
                py_dump(&o[*k], out);
            }
            out.push('}');
        }
    }
}

/// Empreinte du cadrage/temps d'un clip (JSON brut de la sélection) : l'analyse est à refaire
/// si elle change. Identique à la version Python (comparée telle quelle).
pub fn view_key(clip: &Value) -> String {
    let mut m = Map::new();
    for k in ["start", "end", "yaw", "pitch", "roll", "fov", "horizon", "keyframes"] {
        m.insert(k.to_string(), clip.get(k).cloned().unwrap_or(Value::Null));
    }
    py_dumps(&Value::Object(m))
}

/// Vrai si le clip (JSON brut) a une analyse à jour dans les données de sa session.
pub fn is_analyzed(data: &SessionData, clip: &Value) -> bool {
    let id = clip.get("id").and_then(Value::as_str).unwrap_or_default();
    data.get(id).and_then(|e| e.key.as_deref()) == Some(view_key(clip).as_str())
}

// ------------------------------------------------------------------ analyse d'un clip

pub const TRACK_KEEP_S: f64 = 2.0; // suivi image par image : poursuivi jusqu'à 2 s sans nouvelle détection
pub const TRACK_SCORE: f32 = 0.45; // confiance minimale du suiveur (VitTrack)
pub const TAIL_S: f64 = 0.5; // après la dernière détection, le suiveur seul ne tient la zone que 0,5 s
pub const TRACK_MAX: usize = 12; // suiveurs actifs au plus (scènes chargées : village, parking)

/// Position d'une piste : (image, confiance, boîte, détection réelle ?).
type Hit = (usize, f64, BoxF, bool);

/// Positions du suiveur gardées seulement si elles restent cohérentes avec les détections.
///
/// Entre deux détections, le suiveur doit rester à moins d'une taille d'objet de la trajectoire
/// joignant ces détections (sinon il a glissé sur le décor) ; après la dernière, il ne tient
/// que `tail` images, près de la dernière position détectée.
fn consistent(hits: &[Hit], hard: &[Hit], tail: usize, keep: usize) -> Vec<(usize, BoxF)> {
    let hard_f: Vec<f64> = hard.iter().map(|h| h.0 as f64).collect();
    let center = |b: &BoxF| (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
    let mut out = vec![];
    for &(f, _, b, is_hard) in hits {
        if is_hard {
            out.push((f, b));
            continue;
        }
        let k = searchsorted(&hard_f, f as f64);
        let (cx, cy) = center(&b);
        let (ex, ey, size);
        if 0 < k && k < hard.len() {
            let (a, z) = (&hard[k - 1], &hard[k]);
            if z.0 - a.0 > keep {
                continue;
            }
            let u = (f - a.0) as f64 / (z.0 - a.0) as f64;
            let (ac, zc) = (center(&a.2), center(&z.2));
            ex = ac.0 * (1.0 - u) + zc.0 * u;
            ey = ac.1 * (1.0 - u) + zc.1 * u;
            size = a.2[2].max(a.2[3]).max(z.2[2]).max(z.2[3]);
        } else if k == hard.len() && f - hard[hard.len() - 1].0 <= tail {
            let a = &hard[hard.len() - 1];
            (ex, ey) = center(&a.2);
            size = a.2[2].max(a.2[3]);
        } else {
            continue;
        }
        if (cx - ex).hypot(cy - ey) <= size {
            out.push((f, b));
        }
    }
    out
}

struct Active {
    kind: &'static str,
    hits: Vec<Hit>,
    conf: f64,
    lost: usize,
    det: Vec<(usize, BoxF)>,
    bbox: BoxF,
    last_det: usize,
    tracker: VitTracker,
}

fn start_tracker(img: &Image, b: &BoxF) -> VitTracker {
    let [x, y, w, h] = b.map(|v| v.round_ties_even() as i32);
    VitTracker::init(img, [x.max(0), y.max(0), w.max(8), h.max(8)])
}

/// Vignette de revue : zone élargie, 120 px de large.
fn thumb_crop(img: &Image, b: &BoxF) -> Option<Image> {
    let [x, y, w, h] = *b;
    let m = 0.6 * w.max(h);
    let crop = img.crop((x - m).max(0.0) as i64, (y - m).max(0.0) as i64, (x + w + m).min(AW as f64) as i64,
                        (y + h + m).min(AH as f64) as i64);
    if crop.is_empty() {
        return None;
    }
    Some(resize_linear(&crop, 120, ((120 * crop.h) as f64 / crop.w as f64).max(1.0) as usize))
}

/// Écrit une image BGR en JPEG (qualité 95, comme cv2.imwrite).
pub fn write_jpeg(img: &Image, path: &Path) -> Result<()> {
    let rgb = img.swap_rb();
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut f, 95)
        .encode(&rgb.data, img.w as u32, img.h as u32, image::ExtendedColorType::Rgb8)?;
    f.flush()?;
    Ok(())
}

/// Détecte et suit dans le rendu d'analyse (`times`, `mats`, `fovs` : une entrée par image).
///
/// Détection une image sur DETECT_EVERY ; entre deux, chaque objet est suivi image par image
/// (VitTrack), et chaque détection recale son suiveur. Le flou suit donc le mouvement de près
/// et tient l'objet quand la détection le rate un instant (bord d'image, cahots, flou).
/// Retourne les pistes (avec vignette de la meilleure détection, écrite dans `thumbs_dir`).
#[allow(clippy::too_many_arguments)]
pub fn analyze_clip(render: &Path, times: &[f64], mats: &[Mat3], fovs: &[f64], sid: &str, clip_id: &str,
                    detector: &mut Detector, progress: Option<&dyn Fn(f64)>) -> Result<Vec<Track>> {
    let n = times.len();
    let fps = if n > 1 { (n - 1) as f64 / (times[n - 1] - times[0]).max(1e-6) } else { 30.0 };
    let keep = (TRACK_KEEP_S * fps) as usize;
    let mut reader = FrameReader::open(render, AW, AH)?;
    let mut crops: HashMap<(usize, [u64; 4]), Image> = HashMap::new();
    let key = |fi: usize, b: &BoxF| (fi, b.map(f64::to_bits));
    let mut active: Vec<Active> = vec![];
    let mut done: Vec<Active> = vec![];
    for fi in 0..n {
        let Some(img) = reader.next_frame() else { break };
        if fi % DETECT_EVERY != 0 {
            // images intermédiaires : les suiveurs avancent
            for t in active.iter_mut() {
                let (ok, b) = t.tracker.update(&mut detector.tracker, &img)?;
                if ok && t.tracker.score >= TRACK_SCORE && b[2] > 2 && b[3] > 2 {
                    t.bbox = b.map(|v| v as f64);
                    t.hits.push((fi, t.conf * 0.8, t.bbox, false));
                } else {
                    t.lost += 1;
                }
            }
        } else {
            let dets = detector.detect(&img, (fi / DETECT_EVERY).is_multiple_of(TILES_EVERY))?;
            for d in &dets {
                // petite vue de chaque zone, pour la vignette de revue
                if let Some(c) = thumb_crop(&img, &d.bbox) {
                    crops.insert(key(fi, &d.bbox), c);
                }
            }
            // appariement détections ↔ pistes au plus proche de la position prédite (vitesse entre
            // les deux dernières détections) ; sans historique, tolérance large : un objet rapide
            // (voiture croisée, ~150 px entre deux analyses) garde sa piste, deux plaques voisines
            // gardent chacune la leur
            let mut pairs: Vec<(f64, usize, usize)> = vec![];
            for (di, d) in dets.iter().enumerate() {
                let b = d.bbox;
                let (cx, cy) = (b[0] + b[2] / 2.0, b[1] + b[3] / 2.0);
                for (ti, t) in active.iter().enumerate() {
                    if t.kind != d.kind {
                        continue;
                    }
                    let (f1, b1) = t.det[t.det.len() - 1];
                    let obj = b[2].max(b[3]).max(b1[2]).max(b1[3]);
                    let (vx, vy, gate);
                    if t.det.len() > 1 {
                        let (f0, b0) = t.det[t.det.len() - 2];
                        let df = ((f1 - f0) as f64).max(1.0);
                        vx = ((b1[0] + b1[2] / 2.0) - (b0[0] + b0[2] / 2.0)) / df;
                        vy = ((b1[1] + b1[3] / 2.0) - (b0[1] + b0[3] / 2.0)) / df;
                        gate = 1.5 * obj;
                    } else {
                        (vx, vy) = (0.0, 0.0);
                        gate = (4.0 * obj).max(0.18 * AW as f64);
                    }
                    let dt = fi as f64 - f1 as f64;
                    let (px, py) = (b1[0] + b1[2] / 2.0 + vx * dt, b1[1] + b1[3] / 2.0 + vy * dt);
                    let dist = (cx - px).hypot(cy - py);
                    if dist <= gate {
                        pairs.push((dist / gate, di, ti));
                    }
                }
            }
            pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
            let (mut taken_d, mut taken_t) = (vec![false; dets.len()], vec![false; active.len()]);
            let mut matched: HashMap<usize, usize> = HashMap::new();
            for (_, di, ti) in pairs {
                if taken_d[di] || taken_t[ti] {
                    continue;
                }
                taken_d[di] = true;
                taken_t[ti] = true;
                matched.insert(di, ti);
            }
            for (di, d) in dets.iter().enumerate() {
                let ti = match matched.get(&di) {
                    Some(&ti) => {
                        if iou(&d.bbox, &active[ti].bbox) < 0.6 {
                            // dérive du suiveur : on le recale sur la détection
                            active[ti].tracker = start_tracker(&img, &d.bbox);
                        }
                        ti
                    }
                    None => {
                        if active.len() >= TRACK_MAX {
                            continue;
                        }
                        active.push(Active { kind: d.kind, hits: vec![], conf: d.conf, lost: 0, det: vec![], bbox: d.bbox,
                                             last_det: fi, tracker: start_tracker(&img, &d.bbox) });
                        active.len() - 1
                    }
                };
                let t = &mut active[ti];
                t.bbox = d.bbox;
                t.last_det = fi;
                t.lost = 0;
                t.conf = t.conf.max(d.conf);
                t.hits.push((fi, d.conf, d.bbox, true));
                t.det.push((fi, d.bbox));
                if t.det.len() > 2 {
                    t.det.remove(0);
                }
            }
        }
        let mut i = 0;
        while i < active.len() {
            if fi - active[i].last_det > keep || active[i].lost > 3 {
                done.push(active.remove(i));
            } else {
                i += 1;
            }
        }
        if let Some(p) = progress {
            p(fi as f64 / n as f64);
        }
    }
    drop(reader);
    done.extend(active);

    let mut tracks: Vec<Track> = vec![];
    let thumbs = thumbs_dir();
    std::fs::create_dir_all(&thumbs)?;
    for t in done {
        let hard: Vec<Hit> = t.hits.iter().filter(|h| h.3).copied().collect();
        let max_conf = hard.iter().map(|h| h.1).fold(f64::MIN, f64::max);
        if !(hard.len() >= MIN_HITS || max_conf >= SURE_CONF) {
            continue;
        }
        let k = tracks.len();
        let mut best = &hard[0];
        for h in &hard[1..] {
            if h.1 * h.2[2] * h.2[3] > best.1 * best.2[2] * best.2[3] {
                best = h;
            }
        }
        let thumb = format!("{sid}_{clip_id}_{k}.jpg");
        if let Some(c) = crops.get(&key(best.0, &best.2)) {
            write_jpeg(c, &thumbs.join(&thumb))?;
        }
        let conf = round_nd(max_conf, 2);
        let hits = consistent(&t.hits, &hard, (TAIL_S * fps) as usize, keep);
        tracks.push(Track { id: Value::from(k), kind: t.kind.to_string(), conf, enabled: Some(true), thumb: Some(thumb),
                            samples: to_samples(&hits, times, mats, fovs), thumb_conf: conf, extra: Map::new() });
    }
    Ok(merge_fragments(tracks))
}

// ------------------------------------------------------------------ floutage à l'export

/// Zones à flouter par image de sortie [[x, y, w, h]] pour le moteur GPU (même calcul que
/// blur_video, sans décoder ni réencoder la vidéo).
pub fn frame_boxes(times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Track], w: usize, h: usize) -> Vec<Vec<BoxF>> {
    let (wf, hf) = (w as f64, h as f64);
    times.iter().zip(mats).zip(fovs)
        .map(|((t, m), fov)| {
            let mut boxes = vec![];
            for (d, ax, ay) in regions_at(tracks, *t) {
                let Some(b) = sphere_to_box(&d, ax, ay, m, *fov, wf, hf) else { continue };
                let (x0, y0) = (b[0].max(0.0), b[1].max(0.0));
                let (x1, y1) = ((b[0] + b[2]).min(wf), (b[1] + b[3]).min(hf));
                if x1 - x0 >= 2.0 && y1 - y0 >= 2.0 {
                    boxes.push([round_nd(x0, 1), round_nd(y0, 1), round_nd(x1 - x0, 1), round_nd(y1 - y0, 1)]);
                }
            }
            boxes
        })
        .collect()
}

pub const AUTO_NEIGHBORS: usize = 2; // mode détection directe : zones des 2 images voisines ajoutées (pas de clignotement)

fn blur_boxes(img: &mut Image, boxes: &[BoxF], w: usize, h: usize) {
    for b in boxes {
        let (x0, y0) = (b[0].max(0.0) as usize, b[1].max(0.0) as usize);
        let (x1, y1) = ((b[0] + b[2]).min(w as f64).max(0.0) as usize, (b[1] + b[3]).min(h as f64).max(0.0) as usize);
        if x1 < x0 + 2 || y1 < y0 + 2 {
            continue;
        }
        let roi = img.crop(x0 as i64, y0 as i64, x1 as i64, y1 as i64);
        let k = 3.max(((x1 - x0).max(y1 - y0) / 3) | 1);
        img.paste(&gaussian_blur(&gaussian_blur(&roi, k), k), x0, y0);
    }
}

fn frame_rate(src: &Path) -> String {
    Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=r_frame_rate", "-of", "csv=p=0"])
        .arg(src)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "30000/1001".into())
}

/// Floute les zones des pistes dans `src` (une entrée times/mats/fovs par image) → `dst`.
///
/// Avec `detector` (résumé hyperlapse : images trop espacées pour un suivi), chaque image est
/// aussi analysée et ses détections floutées, avec celles des images voisines.
/// Retourne le nombre d'images modifiées. Le son est recopié tel quel.
#[allow(clippy::too_many_arguments)]
pub fn blur_video(src: &Path, dst: &Path, times: &[f64], mats: &[Mat3], fovs: &[f64], tracks: &[Track], w: usize,
                  h: usize, encoder_args: &[String], mut detector: Option<&mut Detector>,
                  progress: Option<&dyn Fn(f64)>) -> Result<usize> {
    let rate = frame_rate(src);
    let mut dec = FrameReader::open(src, w, h)?;
    let mut enc = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "bgr24", "-s", &format!("{w}x{h}"), "-r", &rate,
               "-i", "-", "-i"])
        .arg(src)
        .args(["-map", "0:v", "-map", "1:a?"])
        .args(encoder_args)
        .args(["-c:a", "copy", "-movflags", "+faststart"])
        .arg(dst)
        .stdin(Stdio::piped())
        .spawn()?;
    let mut stdin = enc.stdin.take().context("ffmpeg : pas d'entrée")?;
    let scale = (AW as f64 / w as f64).min(1.0); // détection sur une image ≤ 1920 px de large
    let n = times.len();
    let mut touched = 0usize;
    let mut window: VecDeque<(usize, Image, Vec<BoxF>)> = VecDeque::new();
    let mut detect = |img: &Image| -> Result<Vec<BoxF>> {
        let Some(det) = detector.as_deref_mut() else { return Ok(vec![]) };
        let small = if scale == 1.0 {
            img.clone()
        } else {
            resize_linear(img, (w as f64 * scale) as usize, (h as f64 * scale) as usize)
        };
        Ok(det.detect(&small, true)?
            .into_iter()
            .map(|d| {
                let [x, y, bw, bh] = d.bbox;
                let (cx, cy) = ((x + bw / 2.0) / scale, (y + bh / 2.0) / scale);
                let (hw, hh) = (bw / scale * (1.0 + PAD) / 2.0, bh / scale * (1.0 + PAD) / 2.0);
                [cx - hw, cy - hh, 2.0 * hw, 2.0 * hh]
            })
            .collect())
    };
    let mut emit = |k: usize, window: &VecDeque<(usize, Image, Vec<BoxF>)>, stdin: &mut std::process::ChildStdin| -> Result<()> {
        let j = k.min(n.saturating_sub(1));
        let mut boxes: Vec<BoxF> = if n == 0 {
            vec![]
        } else {
            regions_at(tracks, times[j])
                .into_iter()
                .filter_map(|(d, ax, ay)| sphere_to_box(&d, ax, ay, &mats[j], fovs[j], w as f64, h as f64))
                .collect()
        };
        for (i, _, dets) in window {
            if i.abs_diff(k) <= AUTO_NEIGHBORS {
                boxes.extend(dets.iter().copied());
            }
        }
        let img = &window.iter().find(|x| x.0 == k).context("image absente de la fenêtre")?.1;
        if boxes.is_empty() {
            stdin.write_all(&img.data)?;
        } else {
            let mut img = img.clone();
            blur_boxes(&mut img, &boxes, w, h);
            touched += 1;
            stdin.write_all(&img.data)?;
        }
        Ok(())
    };
    let (mut fi, mut head) = (0usize, 0usize);
    while let Some(img) = dec.next_frame() {
        let dets = detect(&img)?;
        window.push_back((fi, img, dets));
        fi += 1;
        while window.front().is_some_and(|x| (x.0 as i64) < fi as i64 - 2 * AUTO_NEIGHBORS as i64 - 1) {
            window.pop_front();
        }
        // l'image « head » a toutes ses voisines suivantes : on l'écrit
        while head as i64 <= fi as i64 - 1 - AUTO_NEIGHBORS as i64 {
            emit(head, &window, &mut stdin)?;
            head += 1;
        }
        if let (Some(p), true) = (progress, n > 0) {
            p((fi as f64 / n as f64).min(1.0));
        }
    }
    while head < fi {
        emit(head, &window, &mut stdin)?;
        head += 1;
    }
    drop(dec);
    drop(stdin);
    if !enc.wait()?.success() {
        bail!("floutage : échec de l'encodage");
    }
    Ok(touched)
}
