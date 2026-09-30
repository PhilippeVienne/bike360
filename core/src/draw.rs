//! Dessin des incrustations : canevas RGBA flottant, formes anticrénelées et texte (Noto Sans).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use anyhow::{Context, Result};

pub const FONT_BOLD: &str = "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf";
pub const FONT: &str = "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf";
pub const ACCENT: [f32; 3] = [245.0, 165.0, 36.0];

/// Image RGBA (valeurs 0..255 en flottant), ligne par ligne.
#[derive(Clone)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<[f32; 4]>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Canvas { w, h, px: vec![[0.0; 4]; w * h] }
    }

    pub fn filled(w: usize, h: usize, rgba: [f32; 4]) -> Self {
        Canvas { w, h, px: vec![rgba; w * h] }
    }

    pub fn at(&mut self, x: usize, y: usize) -> &mut [f32; 4] {
        &mut self.px[y * self.w + x]
    }

    pub fn to_rgba8(&self) -> image::RgbaImage {
        let mut out = image::RgbaImage::new(self.w as u32, self.h as u32);
        for (o, p) in out.pixels_mut().zip(&self.px) {
            o.0 = p.map(|v| v.clamp(0.0, 255.0) as u8);
        }
        out
    }

    pub fn from_rgba8(img: &image::RgbaImage) -> Self {
        Canvas {
            w: img.width() as usize,
            h: img.height() as usize,
            px: img.pixels().map(|p| p.0.map(|v| v as f32)).collect(),
        }
    }

    pub fn save_png(&self, path: &Path) -> Result<()> {
        self.to_rgba8().save(path).with_context(|| format!("écriture de {path:?}"))
    }

    /// Couvre la zone [x0, x1)×[y0, y1) en appelant `f(x, y)` au centre des pixels (bornée à l'image).
    fn each(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, mut f: impl FnMut(&mut [f32; 4], f64, f64)) {
        let (xa, xb) = (x0.floor().max(0.0) as usize, (x1.ceil().max(0.0) as usize).min(self.w));
        let (ya, yb) = (y0.floor().max(0.0) as usize, (y1.ceil().max(0.0) as usize).min(self.h));
        for y in ya..yb {
            for x in xa..xb {
                let i = y * self.w + x;
                f(&mut self.px[i], x as f64 + 0.5, y as f64 + 0.5);
            }
        }
    }

    /// Remplit une forme donnée par sa distance signée (négative à l'intérieur), en composition « over ».
    pub fn fill_sdf(&mut self, bbox: (f64, f64, f64, f64), color: [f32; 3], alpha: f32, sdf: impl Fn(f64, f64) -> f64) {
        self.each(bbox.0 - 1.0, bbox.1 - 1.0, bbox.2 + 1.0, bbox.3 + 1.0, |p, x, y| {
            let cov = (0.5 - sdf(x, y)).clamp(0.0, 1.0) as f32 * alpha;
            if cov > 0.0 {
                blend(p, color, cov);
            }
        });
    }

    /// Disque plein.
    pub fn disc(&mut self, cx: f64, cy: f64, r: f64, color: [f32; 3], alpha: f32) {
        self.fill_sdf((cx - r, cy - r, cx + r, cy + r), color, alpha, |x, y| (x - cx).hypot(y - cy) - r);
    }

    /// Rectangle plein (bords nets).
    pub fn rect(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, color: [f32; 3], alpha: f32) {
        self.each(x0, y0, x1, y1, |p, _, _| blend(p, color, alpha));
    }

    /// Polyligne épaisse à extrémités et jonctions arrondies, composée « over ».
    pub fn polyline(&mut self, pts: &[(f64, f64)], width: f64, color: [f32; 3], alpha: f32) {
        if pts.len() < 2 {
            return;
        }
        let mut lay = Canvas::new(self.w, self.h);
        lay.stroke(pts, width, color, alpha);
        self.over(&lay, 0, 0);
    }

    /// Trait anticrénelé le long d'une polyligne : l'opacité la plus forte l'emporte (pas
    /// d'accumulation aux jonctions), comme telemetry._stroke.
    pub fn stroke(&mut self, pts: &[(f64, f64)], width: f64, color: [f32; 3], alpha: f32) {
        let r = width / 2.0;
        for seg in pts.windows(2) {
            let ((x0, y0), (x1, y1)) = (seg[0], seg[1]);
            let (dx, dy) = (x1 - x0, y1 - y0);
            let len2 = dx * dx + dy * dy;
            let bbox = (x0.min(x1) - r - 1.0, y0.min(y1) - r - 1.0, x0.max(x1) + r + 2.0, y0.max(y1) + r + 2.0);
            self.each(bbox.0, bbox.1, bbox.2, bbox.3, |p, x, y| {
                let u = if len2 > 0.0 { (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
                let d = (x - (x0 + u * dx)).hypot(y - (y0 + u * dy));
                let cov = ((r + 0.5 - d).clamp(0.0, 1.0)) as f32 * alpha;
                if cov > p[3] / 255.0 {
                    p[0] = color[0];
                    p[1] = color[1];
                    p[2] = color[2];
                    p[3] = cov * 255.0;
                }
            });
        }
    }

    /// Compose `layer` par-dessus (sur place), coin haut gauche en (x, y).
    pub fn over(&mut self, layer: &Canvas, x: i64, y: i64) {
        for ly in 0..layer.h {
            let ty = y + ly as i64;
            if ty < 0 || ty >= self.h as i64 {
                continue;
            }
            for lx in 0..layer.w {
                let tx = x + lx as i64;
                if tx < 0 || tx >= self.w as i64 {
                    continue;
                }
                let l = layer.px[ly * layer.w + lx];
                let b = &mut self.px[ty as usize * self.w + tx as usize];
                let a = l[3] / 255.0;
                for c in 0..3 {
                    b[c] = b[c] * (1.0 - a) + l[c] * a;
                }
                b[3] = b[3].max(l[3]);
            }
        }
    }

    /// Composition alpha standard (Porter-Duff « over », comme PIL alpha_composite).
    pub fn alpha_composite(&mut self, layer: &Canvas, x: i64, y: i64) {
        for ly in 0..layer.h {
            let ty = y + ly as i64;
            if ty < 0 || ty >= self.h as i64 {
                continue;
            }
            for lx in 0..layer.w {
                let tx = x + lx as i64;
                if tx < 0 || tx >= self.w as i64 {
                    continue;
                }
                let s = layer.px[ly * layer.w + lx];
                let d = &mut self.px[ty as usize * self.w + tx as usize];
                let (sa, da) = (s[3] / 255.0, d[3] / 255.0);
                let oa = sa + da * (1.0 - sa);
                if oa > 0.0 {
                    for c in 0..3 {
                        d[c] = (s[c] * sa + d[c] * da * (1.0 - sa)) / oa;
                    }
                }
                d[3] = oa * 255.0;
            }
        }
    }
}

fn blend(p: &mut [f32; 4], color: [f32; 3], cov: f32) {
    let da = p[3] / 255.0;
    let oa = cov + da * (1.0 - cov);
    if oa > 0.0 {
        for c in 0..3 {
            p[c] = (color[c] * cov + p[c] * da * (1.0 - cov)) / oa;
        }
    }
    p[3] = oa * 255.0;
}

/// Opacité d'un rectangle à coins arrondis (1 dedans, bord anticrénelé).
pub fn rounded_coverage(w: usize, h: usize, radius: f64, x: f64, y: f64) -> f64 {
    let dx = (radius - x).max(x - (w as f64 - radius)).max(0.0);
    let dy = (radius - y).max(y - (h as f64 - radius)).max(0.0);
    (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0)
}

/// Panneau sombre translucide à coins arrondis.
pub fn rounded_panel(w: usize, h: usize, radius: f64, alpha: f64) -> Canvas {
    let mut img = Canvas::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.px[y * w + x][3] = (rounded_coverage(w, h, radius, x as f64 + 0.5, y as f64 + 0.5) * alpha * 255.0) as f32;
        }
    }
    img
}

/// Point de position : anneau couleur d'accent, centre blanc.
pub fn dot(size: usize) -> Canvas {
    let mut img = Canvas::new(size, size);
    let s = size as f64;
    for y in 0..size {
        for x in 0..size {
            let d = (x as f64 + 0.5 - s / 2.0).hypot(y as f64 + 0.5 - s / 2.0);
            let ring = (s / 2.0 - d).clamp(0.0, 1.0);
            let core = (s / 2.0 - s * 0.18 - d).clamp(0.0, 1.0);
            let c = if core > 0.0 { [255.0; 3] } else { ACCENT };
            img.px[y * size + x] = [c[0], c[1], c[2], (ring * 255.0) as f32];
        }
    }
    img
}

// ---------------------------------------------------------------- texte

static FONTS: LazyLock<Mutex<HashMap<String, FontArc>>> = LazyLock::new(Default::default);

fn font(path: &str) -> Result<FontArc> {
    let mut cache = FONTS.lock().unwrap();
    if let Some(f) = cache.get(path) {
        return Ok(f.clone());
    }
    let f = FontArc::try_from_vec(std::fs::read(path).with_context(|| format!("police {path}"))?)?;
    cache.insert(path.into(), f.clone());
    Ok(f)
}

/// Échelle telle que la taille corresponde au cadratin en pixels (comme PIL ImageFont.truetype).
fn scale(f: &FontArc, size: f64) -> PxScale {
    let em = f.units_per_em().unwrap_or(1000.0);
    PxScale::from((size * (f.ascent_unscaled() - f.descent_unscaled()) as f64 / em as f64) as f32)
}

/// Glyphes positionnés (ligne de base à y = ascent), avec crénage.
fn layout(f: &FontArc, size: f64, text: &str) -> (Vec<ab_glyph::Glyph>, f32) {
    let sf = f.as_scaled(scale(f, size));
    let mut x = 0.0;
    let mut prev = None;
    let mut out = vec![];
    for ch in text.chars() {
        let id = sf.glyph_id(ch);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        out.push(id.with_scale_and_position(sf.scale(), ab_glyph::point(x, sf.ascent())));
        x += sf.h_advance(id);
        prev = Some(id);
    }
    (out, x)
}

/// Longueur d'avance d'un texte (PIL textlength).
pub fn text_length(font_path: &str, size: f64, text: &str) -> Result<f64> {
    Ok(layout(&font(font_path)?, size, text).1 as f64)
}

/// Boîte de l'encre (x0, y0, x1, y1) d'un texte posé en (0, 0) haut de ligne (PIL textbbox).
pub fn text_bbox(font_path: &str, size: f64, text: &str) -> Result<(i64, i64, i64, i64)> {
    let f = font(font_path)?;
    let (glyphs, adv) = layout(&f, size, text);
    let (mut x0, mut y0, mut x1, mut y1) = (0f32, f32::MAX, adv, f32::MIN);
    for g in glyphs {
        if let Some(o) = f.outline_glyph(g) {
            let b = o.px_bounds();
            x0 = x0.min(b.min.x);
            x1 = x1.max(b.max.x);
            y0 = y0.min(b.min.y);
            y1 = y1.max(b.max.y);
        }
    }
    if y0 > y1 {
        (y0, y1) = (0.0, 0.0);
    }
    Ok((x0.floor() as i64, y0.floor() as i64, x1.ceil() as i64, y1.ceil() as i64))
}

/// Dessine un texte (haut de ligne en (x, y)), composé « over ».
pub fn draw_text(img: &mut Canvas, x: f64, y: f64, text: &str, font_path: &str, size: f64, color: [f32; 3], alpha: f32) -> Result<()> {
    let f = font(font_path)?;
    let (glyphs, _) = layout(&f, size, text);
    for g in glyphs {
        let Some(o) = f.outline_glyph(g) else { continue };
        let b = o.px_bounds();
        o.draw(|gx, gy, c| {
            let (px, py) = ((x + b.min.x as f64).round() as i64 + gx as i64, (y + b.min.y as f64).round() as i64 + gy as i64);
            if px >= 0 && py >= 0 && (px as usize) < img.w && (py as usize) < img.h {
                blend(img.at(px as usize, py as usize), color, c.clamp(0.0, 1.0) * alpha);
            }
        });
    }
    Ok(())
}

/// Texte sur fond transparent, avec ombre portée éventuelle : (image, marge, y0 de l'encre).
pub fn text_sprite(text: &str, font_path: &str, size: f64, color: [f32; 3], shadow: bool) -> Result<(Canvas, i64, i64)> {
    let (x0, y0, x1, y1) = text_bbox(font_path, size, text)?;
    let pad = (size as i64 / 12).max(2) + if shadow { 3 } else { 0 };
    let mut im = Canvas::new((x1 - x0 + 2 * pad) as usize, (y1 - y0 + 2 * pad) as usize);
    if shadow {
        draw_text(&mut im, (pad - x0 + 2) as f64, (pad - y0 + 2) as f64, text, font_path, size, [0.0; 3], 150.0 / 255.0)?;
    }
    draw_text(&mut im, (pad - x0) as f64, (pad - y0) as f64, text, font_path, size, color, 1.0)?;
    Ok((im, pad, y0))
}
