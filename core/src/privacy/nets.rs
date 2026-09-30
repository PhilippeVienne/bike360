//! Réseaux ONNX (ONNX Runtime, carte graphique si possible) : visages YuNet, plaques YOLOv9,
//! véhicules RF-DETR et suiveur VitTrack, avec les pré- et post-traitements d'OpenCV et
//! d'open-image-models reproduits à l'identique.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;

use super::imgproc::{blob, resize_linear, Image};
use crate::paths;

// ------------------------------------------------------------------ environnement ONNX Runtime

/// Bibliothèque onnxruntime : $ORT_DYLIB_PATH, sinon celle d'onnxruntime-gpu installé par pip
/// (1.26 : dernière version pour CUDA 12), sinon celle du système.
fn ort_library() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ORT_DYLIB_PATH") {
        return Some(PathBuf::from(p));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let mut found = vec![];
    for base in [home.join(".local/lib"), PathBuf::from("/usr/lib"), PathBuf::from("/usr/local/lib")] {
        let Ok(dirs) = std::fs::read_dir(&base) else { continue };
        for d in dirs.flatten() {
            let capi = d.path().join("site-packages/onnxruntime/capi");
            let dist = d.path().join("dist-packages/onnxruntime/capi");
            for c in [capi, dist] {
                if let Ok(files) = std::fs::read_dir(&c) {
                    for f in files.flatten() {
                        let n = f.file_name().to_string_lossy().to_string();
                        if n.starts_with("libonnxruntime.so") {
                            found.push(f.path());
                        }
                    }
                }
            }
        }
    }
    found.sort();
    found.pop().or_else(|| {
        ["/usr/lib/x86_64-linux-gnu/libonnxruntime.so", "/usr/local/lib/libonnxruntime.so"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
    })
}

static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn init() -> Result<()> {
    INIT.get_or_init(|| {
        let lib = ort_library().ok_or("bibliothèque onnxruntime introuvable (pip install onnxruntime-gpu==1.26.0)")?;
        ort::init_from(&lib).map_err(|e| format!("{}: {e}", lib.display()))?.with_name("bike360-privacy").commit();
        Ok(())
    })
    .clone()
    .map_err(|e| anyhow!(e))
}

/// Session sur la carte graphique si elle est utilisable, sinon le processeur.
/// `exact` : sans TF32 (sorties à 1e-6 près du calcul processeur d'OpenCV).
fn session(model: &[u8], gpu: bool, exact: bool) -> Result<Session> {
    init()?;
    let mut b = Session::builder()?;
    if gpu {
        b = b.with_execution_providers([ort::ep::CUDA::default().with_tf32(!exact).build()])
            .map_err(|e| anyhow!("fournisseur CUDA : {e}"))?;
    }
    Ok(b.commit_from_memory(model)?)
}

type Outputs = Vec<(Vec<usize>, Vec<f32>)>;

fn run(s: &mut Session, inputs: Vec<(&str, Vec<usize>, Vec<f32>)>, names: &[&str]) -> Result<Outputs> {
    let mut vals = vec![];
    for (n, shape, data) in inputs {
        vals.push((n.to_string(), Tensor::from_array((shape, data))?.into_dyn()));
    }
    let out = s.run(vals)?;
    names.iter()
        .map(|n| {
            let (shape, data) = out.get(n).with_context(|| format!("sortie {n} absente"))?.try_extract_tensor::<f32>()?;
            Ok((shape.iter().map(|d| *d as usize).collect(), data.to_vec()))
        })
        .collect()
}

// ------------------------------------------------------------------ fichiers des modèles

pub const FACE_MODEL: &str = "face_detection_yunet_2023mar.onnx";
pub const TRACK_MODEL: &str = "object_tracking_vittrack_2023sep.onnx";
pub const PLATE_MODEL: &str = "yolo-v9-t-640-license-plate-end2end";
pub const VEHICLE_MODEL: &str = "rf-detr-nano-384-coco";
const OIM_URL: &str = "https://github.com/ankandrew/open-image-models/releases/download/assets";

fn oim_file(name: &str) -> &'static str {
    match name {
        PLATE_MODEL => "yolo-v9-t-640-license-plates-end2end.onnx",
        _ => "rf-detr-nano-384-coco.onnx",
    }
}

/// Modèle du cache du projet (data/cache/models).
pub fn project_model(file: &str) -> Result<PathBuf> {
    let p = paths::cache().join("models").join(file);
    if p.exists() { Ok(p) } else { bail!("modèle {} absent", p.display()) }
}

/// Modèle open-image-models : data/cache/models, sinon ~/.cache/open-image-models (téléchargé au
/// besoin, comme la bibliothèque Python).
pub fn oim_model(name: &str) -> Result<PathBuf> {
    let file = oim_file(name);
    let local = paths::cache().join("models").join(file);
    if local.exists() {
        return Ok(local);
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME")?);
    let dir = home.join(".cache/open-image-models").join(name);
    let p = dir.join(file);
    if !p.exists() {
        std::fs::create_dir_all(&dir)?;
        let resp = ureq::get(&format!("{OIM_URL}/{file}")).call()?;
        let mut buf = vec![];
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
        let tmp = p.with_extension("part");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, &p)?;
    }
    Ok(p)
}

// ------------------------------------------------------------------ protobuf minimal

fn varint(b: &[u8], i: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for s in (0..64).step_by(7) {
        let byte = *b.get(*i).context("protobuf tronqué")?;
        *i += 1;
        v |= ((byte & 0x7f) as u64) << s;
        if byte < 0x80 {
            return Ok(v);
        }
    }
    bail!("varint invalide")
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Champs d'un message : (numéro, type, octets bruts du champ entier, contenu si délimité).
fn fields(b: &[u8]) -> Result<Vec<(u64, u8, &[u8], &[u8])>> {
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let key = varint(b, &mut i)?;
        let (num, wt) = (key >> 3, (key & 7) as u8);
        let payload_start;
        match wt {
            0 => {
                payload_start = i;
                varint(b, &mut i)?;
            }
            1 => {
                payload_start = i;
                i += 8;
            }
            5 => {
                payload_start = i;
                i += 4;
            }
            2 => {
                let n = varint(b, &mut i)? as usize;
                payload_start = i;
                i += n;
            }
            _ => bail!("type protobuf {wt} non géré"),
        }
        if i > b.len() {
            bail!("protobuf tronqué");
        }
        out.push((num, wt, &b[start..i], &b[payload_start..i]));
    }
    Ok(out)
}

fn put_bytes(out: &mut Vec<u8>, num: u64, data: &[u8]) {
    put_varint(out, num << 3 | 2);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// Type tenseur flottant de rang `rank`, dimensions : Some(valeur) fixe ou None (symbolique).
fn tensor_type(dims: &[Option<u64>], tag: &str) -> Vec<u8> {
    let mut shape = vec![];
    for (k, d) in dims.iter().enumerate() {
        let mut dim = vec![];
        match d {
            Some(v) => {
                put_varint(&mut dim, 1 << 3);
                put_varint(&mut dim, *v);
            }
            None => put_bytes(&mut dim, 2, format!("{tag}{k}").as_bytes()),
        }
        put_bytes(&mut shape, 1, &dim);
    }
    let mut tt = vec![];
    put_varint(&mut tt, 1 << 3);
    put_varint(&mut tt, 1); // FLOAT
    put_bytes(&mut tt, 2, &shape);
    let mut ty = vec![];
    put_bytes(&mut ty, 1, &tt);
    ty
}

/// Rang d'un ValueInfoProto (nombre de dimensions déclarées).
fn value_rank(vi: &[u8]) -> Result<usize> {
    for (n, _, _, p) in fields(vi)? {
        if n == 2 {
            for (n, _, _, p) in fields(p)? {
                if n == 1 {
                    for (n, _, _, p) in fields(p)? {
                        if n == 2 {
                            return Ok(fields(p)?.iter().filter(|f| f.0 == 1).count());
                        }
                    }
                }
            }
        }
    }
    Ok(0)
}

fn retype(vi: &[u8], dims: &[Option<u64>], tag: &str) -> Result<Vec<u8>> {
    let mut out = vec![];
    for (n, _, raw, _) in fields(vi)? {
        if n == 2 {
            put_bytes(&mut out, 2, &tensor_type(dims, tag));
        } else {
            out.extend_from_slice(raw);
        }
    }
    Ok(out)
}

/// YuNet déclare une entrée fixe 640×640 ; OpenCV la redimensionne librement (réseau entièrement
/// convolutif). On rend hauteur et largeur symboliques et on retire les formes intermédiaires.
fn dynamic_yunet(model: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![];
    for (n, _, raw, graph) in fields(model)? {
        if n != 7 {
            out.extend_from_slice(raw);
            continue;
        }
        let mut g = vec![];
        for (gn, _, graw, p) in fields(graph)? {
            match gn {
                13 => {} // value_info : formes intermédiaires figées
                11 => {
                    let is_input = fields(p)?.iter().any(|f| f.0 == 1 && f.3 == b"input");
                    if is_input {
                        put_bytes(&mut g, 11, &retype(p, &[Some(1), Some(3), None, None], "in")?);
                    } else {
                        g.extend_from_slice(graw);
                    }
                }
                12 => {
                    let rank = value_rank(p)?;
                    put_bytes(&mut g, 12, &retype(p, &vec![None; rank], "out")?);
                }
                _ => g.extend_from_slice(graw),
            }
        }
        put_bytes(&mut out, 7, &g);
    }
    Ok(out)
}

// ------------------------------------------------------------------ visages : YuNet

/// Détecteur de visages YuNet (cv2.FaceDetectorYN) : [x, y, w, h, score] dans l'image donnée.
pub struct YuNet {
    s: Session,
    score: f32,
    nms: f32,
    top_k: usize,
}

impl YuNet {
    pub fn new(path: &Path, gpu: bool, score: f32, nms: f32, top_k: usize) -> Result<Self> {
        let model = dynamic_yunet(&std::fs::read(path)?)?;
        Ok(YuNet { s: session(&model, gpu, true)?, score, nms, top_k })
    }

    pub fn detect(&mut self, img: &Image) -> Result<Vec<[f32; 5]>> {
        let pw = (img.w - 1) / 32 * 32 + 32;
        let ph = (img.h - 1) / 32 * 32 + 32;
        let pad = img.border(0, ph - img.h, 0, pw - img.w, 0);
        let input = blob(&pad, [0.0; 3], [1.0; 3], false);
        let names = ["cls_8", "cls_16", "cls_32", "obj_8", "obj_16", "obj_32", "bbox_8", "bbox_16", "bbox_32"];
        let out = run(&mut self.s, vec![("input", vec![1, 3, ph, pw], input)], &names)?;
        let mut faces: Vec<[f32; 5]> = vec![];
        for (i, stride) in [8usize, 16, 32].into_iter().enumerate() {
            let (cols, rows) = (pw / stride, ph / stride);
            let (cls, obj, bbox) = (&out[i].1, &out[i + 3].1, &out[i + 6].1);
            let st = stride as f32;
            for r in 0..rows {
                for c in 0..cols {
                    let idx = r * cols + c;
                    let score = (cls[idx].clamp(0.0, 1.0) * obj[idx].clamp(0.0, 1.0)).sqrt();
                    if score < self.score {
                        continue;
                    }
                    let cx = (c as f32 + bbox[idx * 4]) * st;
                    let cy = (r as f32 + bbox[idx * 4 + 1]) * st;
                    let w = bbox[idx * 4 + 2].exp() * st;
                    let h = bbox[idx * 4 + 3].exp() * st;
                    faces.push([cx - w / 2.0, cy - h / 2.0, w, h, score]);
                }
            }
        }
        if faces.len() <= 1 {
            return Ok(faces);
        }
        let rects: Vec<[i32; 4]> = faces.iter().map(|f| [f[0] as i32, f[1] as i32, f[2] as i32, f[3] as i32]).collect();
        let scores: Vec<f32> = faces.iter().map(|f| f[4]).collect();
        Ok(nms_boxes(&rects, &scores, self.score, self.nms, self.top_k).into_iter().map(|k| faces[k]).collect())
    }
}

/// cv::dnn::NMSBoxes (boîtes entières, eta = 1).
fn nms_boxes(rects: &[[i32; 4]], scores: &[f32], thr: f32, nms: f32, top_k: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..scores.len()).filter(|&i| scores[i] > thr).collect();
    order.sort_by(|a, b| scores[*b].total_cmp(&scores[*a])); // tri stable, décroissant
    if top_k > 0 && top_k < order.len() {
        order.truncate(top_k);
    }
    let area = |r: &[i32; 4]| r[2] as i64 * r[3] as i64;
    let overlap = |a: &[i32; 4], b: &[i32; 4]| -> f32 {
        let (aa, ab) = (area(a), area(b));
        if aa + ab <= 0 {
            return 1.0;
        }
        let ix = (a[0] + a[2]).min(b[0] + b[2]) - a[0].max(b[0]);
        let iy = (a[1] + a[3]).min(b[1] + b[3]) - a[1].max(b[1]);
        let inter = if ix > 0 && iy > 0 { ix as f64 * iy as f64 } else { 0.0 };
        1.0 - (1.0 - inter / ((aa + ab) as f64 - inter)) as f32
    };
    let mut keep: Vec<usize> = vec![];
    for i in order {
        if keep.iter().all(|&k| overlap(&rects[i], &rects[k]) <= nms) {
            keep.push(i);
        }
    }
    keep
}

// ------------------------------------------------------------------ plaques : YOLOv9 (bout en bout)

/// Détection brute d'open-image-models : boîte entière (x1, y1, x2, y2), classe, confiance.
#[derive(Clone, Copy, Debug)]
pub struct Det {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub class: i64,
    pub conf: f32,
}

impl Det {
    pub fn w(&self) -> i32 {
        self.x2 - self.x1
    }
    pub fn h(&self) -> i32 {
        self.y2 - self.y1
    }
}

pub struct Yolo {
    s: Session,
    size: usize,
    conf: f32,
}

impl Yolo {
    pub fn new(path: &Path, gpu: bool, conf: f32) -> Result<Self> {
        Ok(Yolo { s: session(&std::fs::read(path)?, gpu, false)?, size: 640, conf })
    }

    /// letterbox (open-image-models) + blobFromImage(1/255, swapRB) + NMS intégré au modèle.
    pub fn detect(&mut self, img: &Image) -> Result<Vec<Det>> {
        let n = self.size as f64;
        let r = (n / img.h as f64).min(n / img.w as f64);
        let (nw, nh) = ((img.w as f64 * r).round_ties_even() as usize, (img.h as f64 * r).round_ties_even() as usize);
        let (dw, dh) = ((n - nw as f64) / 2.0, (n - nh as f64) / 2.0);
        let im = if (img.w, img.h) != (nw, nh) { resize_linear(img, nw, nh) } else { img.clone() };
        let (top, bottom) = ((dh - 0.1).round_ties_even() as usize, (dh + 0.1).round_ties_even() as usize);
        let (left, right) = ((dw - 0.1).round_ties_even() as usize, (dw + 0.1).round_ties_even() as usize);
        let im = im.border(top, bottom, left, right, 114);
        let s = (1.0 / 255.0) as f32;
        let input = blob(&im, [0.0; 3], [s; 3], true);
        let out = run(&mut self.s, vec![("images", vec![1, 3, im.h, im.w], input)], &["output0"])?;
        let (shape, v) = &out[0];
        let cols = *shape.last().unwrap_or(&7);
        let (rf, pw, ph) = (r as f32, dw as f32, dh as f32);
        let mut dets = vec![];
        for p in v.chunks_exact(cols) {
            if p[6] < self.conf {
                continue;
            }
            dets.push(Det {
                x1: ((p[1] - pw) / rf) as i32,
                y1: ((p[2] - ph) / rf) as i32,
                x2: ((p[3] - pw) / rf) as i32,
                y2: ((p[4] - ph) / rf) as i32,
                class: p[5] as i64,
                conf: p[6],
            });
        }
        Ok(dets)
    }
}

// ------------------------------------------------------------------ véhicules : RF-DETR

/// Identifiants COCO utilisés par open-image-models (91 cases, 80 classes).
const COCO_IDS: [i64; 80] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 27, 28, 31, 32, 33, 34, 35,
    36, 37, 38, 39, 40, 41, 42, 43, 44, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65,
    67, 70, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 84, 85, 86, 87, 88, 89, 90,
];
pub const COCO_MOTORCYCLE: i64 = 4;

pub struct RfDetr {
    s: Session,
    size: usize,
    conf: f32,
}

impl RfDetr {
    pub fn new(path: &Path, gpu: bool, conf: f32) -> Result<Self> {
        Ok(RfDetr { s: session(&std::fs::read(path)?, gpu, false)?, size: 384, conf })
    }

    pub fn detect(&mut self, img: &Image) -> Result<Vec<Det>> {
        let sz = self.size;
        let im = resize_linear(img, sz, sz);
        let mean = [0.485f32, 0.456, 0.406];
        let std = [0.229f32, 0.224, 0.225];
        // (x/255 − moyenne)/écart-type, en RVB ; même ordre d'opérations que NumPy (float32)
        let plane = sz * sz;
        let mut input = vec![0f32; 3 * plane];
        for (i, p) in im.data.chunks_exact(3).enumerate() {
            for d in 0..3 {
                input[d * plane + i] = (p[2 - d] as f32 / 255.0 - mean[d]) / std[d];
            }
        }
        let out = run(&mut self.s, vec![("input", vec![1, 3, sz, sz], input)], &["dets", "labels"])?;
        let (boxes, (lshape, logits)) = (&out[0].1, (&out[1].0, &out[1].1));
        let nc = lshape[2];
        let q = lshape[1];
        // meilleures paires (requête, classe) parmi les 80 classes, décroissantes
        let mut flat: Vec<(f32, usize, i64)> = Vec::with_capacity(q * COCO_IDS.len());
        for qi in 0..q {
            for &c in &COCO_IDS {
                flat.push((logits[qi * nc + c as usize], qi, c));
            }
        }
        flat.sort_by(|a, b| b.0.total_cmp(&a.0));
        flat.truncate(300);
        let (ih, iw) = (img.h as f32, img.w as f32);
        let mut dets = vec![];
        for (lg, qi, c) in flat {
            let score = 1.0 / (1.0 + (-lg.clamp(-88.0, 88.0)).exp());
            if score <= self.conf {
                continue;
            }
            let b = &boxes[qi * 4..qi * 4 + 4];
            let (hw, hh) = (b[2].max(0.0) / 2.0, b[3].max(0.0) / 2.0);
            let xyxy = [(b[0] - hw) * iw, (b[1] - hh) * ih, (b[0] + hw) * iw, (b[1] + hh) * ih];
            let lim = [iw, ih, iw, ih];
            let v: [i32; 4] = [0, 1, 2, 3].map(|k| xyxy[k].clamp(0.0, lim[k]) as i32);
            let d = Det { x1: v[0], y1: v[1], x2: v[2], y2: v[3], class: c, conf: score };
            if d.w() > 0 && d.h() > 0 && d.x2 <= img.w as i32 && d.y2 <= img.h as i32 {
                dets.push(d);
            }
        }
        Ok(dets)
    }
}

// ------------------------------------------------------------------ suivi : VitTrack

/// Réseau VitTrack partagé par tous les suiveurs.
pub struct VitNet {
    s: Session,
}

impl VitNet {
    pub fn new(path: &Path, gpu: bool) -> Result<Self> {
        Ok(VitNet { s: session(&std::fs::read(path)?, gpu, true)? })
    }
}

/// Suiveur (cv::TrackerVit d'OpenCV 5) : gabarit fixé à l'initialisation, recherche autour de
/// la dernière position, fenêtre de Hann sur la carte de confiance.
#[derive(Clone)]
pub struct VitTracker {
    template: Vec<f32>,
    pub rect: [i32; 4],
    pub score: f32,
}

const VIT_THRESHOLD: f32 = 0.20;

fn vit_crop(img: &Image, b: [i32; 4], factor: i32) -> (Image, i32) {
    let [x, y, w, h] = b;
    let crop_sz = ((w as f64 * h as f64).sqrt() * factor as f64).ceil() as i32;
    let x1 = x + (w - crop_sz) / 2;
    let x2 = x1 + crop_sz;
    let y1 = y + (h - crop_sz) / 2;
    let y2 = y1 + crop_sz;
    let x1p = (-x1).max(0);
    let y1p = (-y1).max(0);
    let x2p = (x2 - img.w as i32 + 1).max(0);
    let y2p = (y2 - img.h as i32 + 1).max(0);
    let (rx, ry) = (x1 + x1p, y1 + y1p);
    let (rw, rh) = (x2 - x2p - x1 - x1p, y2 - y2p - y1 - y1p);
    if rw <= 0 || rh <= 0 {
        return (Image::new(0, 0, 3), crop_sz);
    }
    // copyMakeBorder reçoit une sous-matrice : sans BORDER_ISOLATED, OpenCV prend les vrais pixels
    // voisins pour la bordure tant qu'il y en a (ici la dernière colonne/ligne écartée par le
    // « + 1 » des marges droite et basse), puis des zéros : le recadrage vaut donc l'image sur
    // [x1, x2) × [y1, y2) complétée de zéros hors du cadre.
    let (ex, ey) = ((img.w as i32 - rx - rw).min(x2p), (img.h as i32 - ry - rh).min(y2p));
    let roi = img.crop(rx as i64, ry as i64, (rx + rw + ex) as i64, (ry + rh + ey) as i64);
    (roi.border(y1p as usize, (y2p - ey) as usize, x1p as usize, (x2p - ex) as usize, 0), crop_sz)
}

fn vit_blob(img: &Image, size: usize) -> Vec<f32> {
    let im = resize_linear(img, size, size);
    let mean = [0.485f64, 0.456, 0.406].map(|m| (m * 255.0) as f32);
    // OpenCV calcule `(1.0 / stdvalue) * (1 / 255.0)` sur un cv::Scalar : `double / Scalar` y est
    // une division de quaternion (conjugué / norme²), d'où des échelles négatives sur les canaux 1
    // et 2. Le réseau a été utilisé ainsi par la version Python : on reproduit ce comportement.
    let std = [0.229f64, 0.224, 0.225, 0.0];
    let n2: f64 = std.iter().map(|v| v * v).sum();
    let s = 1.0 / n2;
    let scale = [std[0] * s, -std[1] * s, -std[2] * s].map(|v| (v * (1.0 / 255.0)) as f32);
    blob(&im, mean, scale, false)
}

fn hann16() -> [f32; 16] {
    std::array::from_fn(|i| 0.5 * (1.0 - ((2.0 * std::f64::consts::PI / 17.0) as f32 * (i + 1) as f32).cos()))
}

impl VitTracker {
    pub fn init(img: &Image, b: [i32; 4]) -> Self {
        let (crop, _) = vit_crop(img, b, 2);
        let template = if crop.is_empty() { vec![0.0; 3 * 128 * 128] } else { vit_blob(&crop, 128) };
        VitTracker { template, rect: b, score: 0.0 }
    }

    /// (succès, boîte) ; le score reste consultable dans `self.score`.
    pub fn update(&mut self, net: &mut VitNet, img: &Image) -> Result<(bool, [i32; 4])> {
        let (crop, crop_size) = vit_crop(img, self.rect, 4);
        if crop.is_empty() {
            self.score = 0.0;
            return Ok((false, [0; 4]));
        }
        let search = vit_blob(&crop, 256);
        let out = run(&mut net.s, vec![("template", vec![1, 3, 128, 128], self.template.clone()),
                                       ("search", vec![1, 3, 256, 256], search)],
                      &["output1", "output2", "output3"])?;
        let (conf, size, offset) = (&out[0].1, &out[1].1, &out[2].1);
        let h = hann16();
        let (mut best, mut bi) = (f32::NEG_INFINITY, 0usize);
        for (i, c) in conf.iter().enumerate().take(256) {
            let v = c * (h[i / 16] * h[i % 16]);
            if v > best {
                best = v;
                bi = i;
            }
        }
        self.score = best;
        if best < VIT_THRESHOLD {
            return Ok((false, [0; 4]));
        }
        let (my, mx) = (bi / 16, bi % 16);
        let cx = (mx as f32 + offset[bi]) / 16.0;
        let cy = (my as f32 + offset[256 + bi]) / 16.0;
        let (w, hh) = (size[bi], size[256 + bi]);
        let [rx, ry, rw, rh] = self.rect;
        let x0 = rx + (rw - crop_size) / 2;
        let y0 = ry + (rh - crop_size) / 2;
        let (x1, y1) = (cx - w / 2.0, cy - hh / 2.0);
        let cs = crop_size as f32;
        self.rect = [
            (x1 * cs + x0 as f32).floor() as i32,
            (y1 * cs + y0 as f32).floor() as i32,
            (w * cs).floor() as i32,
            (hh * cs).floor() as i32,
        ];
        Ok((true, self.rect))
    }
}
