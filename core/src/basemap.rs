//! Fond de carte raster sombre pour les cartes des exports (tuiles OpenStreetMap, inversées).
//!
//! Les tuiles sont téléchargées à la demande et gardées dans data/cache/tiles/osm/{z}/{x}/{y}.png
//! (même arborescence que la version Python) ; l'image doit porter l'attribution [`ATTRIBUTION`].
//! Projection Web Mercator, comme les tuiles.
//!
//! API :
//! - [`render`]`(lats, lons, out_size, pad, max_fill) -> Option<(RgbImage, Projector)>` :
//!   carte carrée `out_size`² englobant les points (NaN ignorés), None si tuiles indisponibles
//!   (hors ligne) ou moins de deux points. Valeurs Python par défaut : pad 0.12, max_fill 1.0.
//! - [`Projector::project`]`(lat, lon) -> (x, y)` : pixels dans l'image (NaN → NaN).
//! - [`stylize`] : assombrissement de l'image (sur place), [`world`] : coordonnées monde.

use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use image::{Rgb, RgbImage};
use serde::Serialize;

use crate::paths;

pub const TILE_URL: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
pub const TILE: i64 = 256;
pub const MAX_ZOOM: u32 = 16;
/// Carte agrandie : noms de lieux lisibles une fois incrustés dans la vidéo.
pub const UPSCALE: f64 = 1.4;
/// Une ligne chacune (mini-carte étroite).
pub const ATTRIBUTION: &[&str] = &["© contributeurs OpenStreetMap"];
pub const USER_AGENT: &str = "bike360/1.0 (outil personnel de montage)";

/// Dossier des tuiles en cache.
pub fn cache_dir() -> PathBuf {
    paths::cache().join("tiles").join("osm")
}

/// Fond sombre et sobre : carte OSM inversée, teinte retournée (l'eau reste bleue, les forêts
/// vert sombre), couleurs atténuées. Routes et noms deviennent clairs sur fond sombre.
pub fn stylize(img: &mut RgbImage, saturation: f32, brightness: f32) {
    for p in img.pixels_mut() {
        let inv = p.0.map(|v| 255.0 - v as f32);
        let g = inv[0] * 0.299 + inv[1] * 0.587 + inv[2] * 0.114;
        p.0 = inv.map(|c| ((g - (c - g) * saturation) * brightness + 8.0).clamp(0.0, 255.0) as u8);
    }
}

/// Coordonnées pixel (monde) Web Mercator au zoom z.
pub fn world(lat: f64, lon: f64, z: u32) -> (f64, f64) {
    let n = (TILE * (1i64 << z)) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let s = lat.clamp(-85.0, 85.0).to_radians().sin();
    let y = (0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI)) * n;
    (x, y)
}

/// Projection (lat, lon) → pixels de l'image rendue.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Projector {
    pub z: u32,
    /// Coin haut gauche de l'image en coordonnées monde (avant agrandissement).
    pub origin_x: f64,
    pub origin_y: f64,
    /// Facteur d'agrandissement out_size / taille recadrée.
    pub k: f64,
}

impl Projector {
    pub fn project(&self, lat: f64, lon: f64) -> (f64, f64) {
        let (px, py) = world(lat, lon, self.z);
        ((px - self.origin_x) * self.k, (py - self.origin_y) * self.k)
    }

    /// Projection de séries entières.
    pub fn project_all(&self, lats: &[f64], lons: &[f64]) -> (Vec<f64>, Vec<f64>) {
        lats.iter().zip(lons).map(|(&a, &b)| self.project(a, b)).unzip()
    }
}

/// Tuile (z, x, y) en RGB, téléchargée si absente du cache ; transparence sur fond clair.
pub fn tile(agent: &ureq::Agent, z: u32, x: i64, y: i64) -> Result<RgbImage> {
    let path = cache_dir().join(z.to_string()).join(x.to_string()).join(format!("{y}.png"));
    if !path.exists() {
        let url = TILE_URL.replace("{z}", &z.to_string()).replace("{x}", &x.to_string()).replace("{y}", &y.to_string());
        let mut data = vec![];
        agent.get(&url).call().with_context(|| url.clone())?.into_reader().read_to_end(&mut data)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, &data)?;
    }
    let im = image::load_from_memory(&std::fs::read(&path)?).with_context(|| format!("{path:?}"))?.to_rgba8();
    let bg = [242.0, 239.0, 233.0];
    let mut out = RgbImage::new(im.width(), im.height());
    for (o, p) in out.pixels_mut().zip(im.pixels()) {
        let a = p.0[3] as f32 / 255.0;
        o.0 = [0, 1, 2].map(|c| (p.0[c] as f32 * a + bg[c] * (1.0 - a)).round() as u8);
    }
    Ok(out)
}

fn ptp(v: &[f64]) -> f64 {
    let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)));
    hi - lo
}

fn min_max(v: &[f64]) -> (f64, f64) {
    v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), &x| (a.min(x), b.max(x)))
}

/// Carte carrée de `out_size` px englobant les points (lat, lon) ; None si tuiles indisponibles.
///
/// `max_fill` > 1 : agrandit encore jusqu'à ce facteur pour que les tracés remplissent la carte
/// (le zoom des tuiles est entier : sans cela la carte peut être deux fois trop large).
pub fn render(lats: &[f64], lons: &[f64], out_size: u32, pad: f64, max_fill: f64) -> Option<(RgbImage, Projector)> {
    let mut size = (out_size as f64 / UPSCALE).round_ties_even() as i64;
    let (lats, lons): (Vec<f64>, Vec<f64>) =
        lats.iter().zip(lons).filter(|(a, b)| !(a.is_nan() || b.is_nan())).map(|(a, b)| (*a, *b)).unzip();
    if lats.len() < 2 {
        return None;
    }
    let usable = size as f64 * (1.0 - 2.0 * pad);
    let project = |z: u32| -> (Vec<f64>, Vec<f64>) { lats.iter().zip(&lons).map(|(&a, &b)| world(a, b, z)).unzip() };
    let mut z = MAX_ZOOM;
    while z > 1 {
        let (x, y) = project(z);
        if ptp(&x).max(ptp(&y)) <= usable {
            break;
        }
        z -= 1;
    }
    let (x, y) = project(z);
    let fill = 1f64.max(max_fill.min(usable / ptp(&x).max(ptp(&y)).max(1e-9)));
    size = 16.max((size as f64 / fill).round_ties_even() as i64);
    let ((xa, xb), (ya, yb)) = (min_max(&x), min_max(&y));
    let (cx, cy) = ((xa + xb) / 2.0, (ya + yb) / 2.0);
    let (x0, y0) = (cx - size as f64 / 2.0, cy - size as f64 / 2.0);
    let t = TILE as f64;
    let (tx0, ty0) = ((x0 / t).floor() as i64, (y0 / t).floor() as i64);
    let (tx1, ty1) = (((x0 + size as f64) / t).floor() as i64, ((y0 + size as f64) / t).floor() as i64);
    let keys: Vec<(i64, i64)> = (ty0..=ty1).flat_map(|ty| (tx0..=tx1).map(move |tx| (tx, ty))).collect();

    // téléchargement parallèle (4 fils), comme ThreadPoolExecutor(4)
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(20)).user_agent(USER_AGENT).build();
    let wrap = 1i64 << z;
    let tiles: Vec<Result<RgbImage>> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..4)
            .map(|j| {
                let (keys, agent) = (&keys, &agent);
                sc.spawn(move || {
                    keys.iter().enumerate().skip(j).step_by(4)
                        .map(|(i, &(tx, ty))| (i, tile(agent, z, tx.rem_euclid(wrap), ty)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<(usize, Result<RgbImage>)> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        all.sort_by_key(|(i, _)| *i);
        all.into_iter().map(|(_, r)| r).collect()
    });
    let tiles = match tiles.into_iter().collect::<Result<Vec<_>>>() {
        Ok(t) => t,
        Err(e) => {   // hors ligne, service indisponible : mini-carte simple
            eprintln!("  fond de carte indisponible : {e:#}");
            return None;
        }
    };
    let (mw, mh) = ((tx1 - tx0 + 1) * TILE, (ty1 - ty0 + 1) * TILE);
    let mut mosaic = RgbImage::new(mw as u32, mh as u32);
    for (&(tx, ty), im) in keys.iter().zip(&tiles) {
        image::imageops::replace(&mut mosaic, im, (tx - tx0) * TILE, (ty - ty0) * TILE);
    }
    let ox = (x0 - (tx0 * TILE) as f64).round_ties_even() as i64;
    let oy = (y0 - (ty0 * TILE) as f64).round_ties_even() as i64;
    // recadrage (zones hors mosaïque en noir, comme PIL crop)
    let mut crop = RgbImage::new(size as u32, size as u32);
    for (cx, cy, p) in crop.enumerate_pixels_mut() {
        let (sx, sy) = (ox + cx as i64, oy + cy as i64);
        *p = if sx >= 0 && sy >= 0 && sx < mw && sy < mh { *mosaic.get_pixel(sx as u32, sy as u32) } else { Rgb([0, 0, 0]) };
    }
    let mut img = image::imageops::resize(&crop, out_size, out_size, image::imageops::FilterType::Lanczos3);
    stylize(&mut img, 0.4, 0.72);
    let k = out_size as f64 / size as f64;
    Some((img, Projector { z, origin_x: (tx0 * TILE + ox) as f64, origin_y: (ty0 * TILE + oy) as f64, k }))
}
