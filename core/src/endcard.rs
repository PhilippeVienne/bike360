//! Carte de fin d'un montage : carte des balades et statistiques cumulées (image PNG).
//!
//! Les statistiques viennent de l'analyse (champ `stats`) de chaque session du montage :
//! distances, temps de roulage et dénivelés additionnés ; vitesse et altitude maximales.
//!
//! API :
//! - [`summary`]`(results) -> Summary` (jours en heure locale, comme la version Python).
//! - [`date_label`]`(days) -> String` (« Samedi 29 août 2026 », « Du 3 au 5 août 2026 »…).
//! - [`map`]`(results, size) -> Option<Canvas>` : carte carrée des tracés (None sans GPS ou hors ligne).
//! - [`render`]`(results, W, H, path, title, credits) -> Result<Summary>` : image W×H → `path`.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::Serialize;
use serde_json::Value;

use crate::analyze::Analysis;
use crate::basemap;
use crate::draw::{self, Canvas, ACCENT, FONT, FONT_BOLD};
use crate::telemetry::raw;

pub const MONTHS: [&str; 12] = ["janvier", "février", "mars", "avril", "mai", "juin", "juillet", "août", "septembre",
                                "octobre", "novembre", "décembre"];
pub const DAYS: [&str; 7] = ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"];
pub const BG: [f32; 3] = [17.0, 19.0, 23.0];

/// Statistiques cumulées des sessions.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub days: Vec<NaiveDate>,
    pub distance_km: f64,
    pub moving_s: f64,
    pub climb_m: f64,
    pub max_speed_kmh: Option<f64>,
    pub alt_max_m: Option<f64>,
}

/// Entier avec espaces fines insécables (U+202F) entre milliers, comme la version Python.
pub fn num(x: f64) -> String {
    let s = format!("{x:.0}");
    let (sign, digits) = s.strip_prefix('-').map_or(("", s.as_str()), |d| ("-", d));
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{202f}');   // espace fine insécable, comme la version Python
        }
        out.push(c);
    }
    format!("{sign}{out}")
}

/// Statistiques cumulées des sessions (liste d'analyses).
pub fn summary(results: &[&Analysis]) -> Summary {
    let get = |r: &Analysis, k: &str| r.stats.get(k).and_then(Value::as_f64);
    let tot = |k: &str| results.iter().map(|r| get(r, k).unwrap_or(0.0)).sum::<f64>();
    let top = |k: &str| results.iter().filter_map(|r| get(r, k)).reduce(|a, b| if b > a { b } else { a });
    let mut days: Vec<NaiveDate> = results.iter()
        .filter_map(|r| DateTime::from_timestamp_micros((r.utc_t0 * 1e6).round() as i64))
        .map(|d| d.with_timezone(&Local).date_naive())
        .collect();
    days.sort();
    days.dedup();
    Summary { days, distance_km: tot("distance_km"), moving_s: tot("moving_s"), climb_m: tot("climb_m"),
              max_speed_kmh: top("max_speed_kmh"), alt_max_m: top("alt_max_m") }
}

/// Date ou période du montage, en toutes lettres.
pub fn date_label(days: &[NaiveDate]) -> String {
    let (Some(a), Some(b)) = (days.first(), days.last()) else { return String::new() };
    let month = |d: &NaiveDate| MONTHS[d.month0() as usize];
    if a == b {
        let wd = DAYS[a.weekday().num_days_from_monday() as usize];
        let mut cap = wd.chars();
        let wd: String = cap.next().map(|c| c.to_uppercase().chain(cap).collect()).unwrap_or_default();
        return format!("{wd} {} {} {}", a.day(), month(a), a.year());
    }
    if (a.month(), a.year()) == (b.month(), b.year()) {
        return format!("Du {} au {} {} {}", a.day(), b.day(), month(b), b.year());
    }
    format!("Du {} {} au {} {} {}", a.day(), month(a), b.day(), month(b), b.year())
}

/// Coordonnées PIL (centre des pixels aux entiers) → canevas (centres en +0,5).
const PIL: f64 = 0.5;

/// Carte carrée des tracés (fond topographique si disponible), ou None sans GPS.
pub fn map(results: &[&Analysis], size: usize) -> Option<Canvas> {
    let tracks: Vec<(Vec<f64>, Vec<f64>)> = results.iter()
        .map(|r| (raw(r, "lat"), raw(r, "lon")))
        .filter(|(lat, _)| lat.iter().filter(|v| v.is_finite()).count() > 10)
        .collect();
    if tracks.is_empty() {
        return None;
    }
    let lats: Vec<f64> = tracks.iter().flat_map(|t| t.0.iter().copied()).collect();
    let lons: Vec<f64> = tracks.iter().flat_map(|t| t.1.iter().copied()).collect();
    let (img, project) = basemap::render(&lats, &lons, size as u32, 0.08, 2.0)?;
    let mut im = Canvas::new(size, size);
    for (p, q) in im.px.iter_mut().zip(img.pixels()) {
        *p = [q.0[0] as f32, q.0[1] as f32, q.0[2] as f32, 255.0];
    }
    let w = (size / 80).max(5) as f64;
    let dark = [25.0, 25.0, 35.0];
    let mut ends = vec![];
    for (lat, lon) in &tracks {
        let (x, y) = project.project_all(lat, lon);
        let mut draw_run = |run: &[(f64, f64)]| {
            im.polyline(run, w + 4.0, dark, 1.0);
            im.polyline(run, w, ACCENT, 1.0);
        };
        let mut run: Vec<(f64, f64)> = vec![];
        for (&px, &py) in x.iter().zip(&y) {
            if px.is_finite() && py.is_finite() {
                run.push((px + PIL, py + PIL));
            } else {
                if run.len() > 1 {
                    draw_run(&run);
                }
                run.clear();
            }
        }
        if run.len() > 1 {
            draw_run(&run);
        }
        let pts: Vec<(f64, f64)> = x.iter().zip(&y).filter(|(a, b)| a.is_finite() && b.is_finite()).map(|(a, b)| (*a, *b)).collect();
        if let (Some(s), Some(e)) = (pts.first(), pts.last()) {
            ends.push((*s, *e));
        }
    }
    // départ (vert) et arrivée (sombre, centre blanc) de chaque balade
    let r = (size / 60).max(6) as f64;
    let ow = ((r as usize) / 3).max(2) as f64;
    for ((sx, sy), (ex, ey)) in ends {
        let (sx, sy, ex, ey) = (sx + PIL, sy + PIL, ex + PIL, ey + PIL);
        im.disc(sx, sy, r + 0.5, [255.0; 3], 1.0);
        im.disc(sx, sy, r + 0.5 - ow, [46.0, 170.0, 90.0], 1.0);
        im.disc(ex, ey, r + 0.5, [255.0; 3], 1.0);
        im.disc(ex, ey, r + 0.5 - ow, dark, 1.0);
        im.disc(ex, ey, r / 3.0 + 0.5, [255.0; 3], 1.0);
    }
    let fs = (size / 55).max(10) as f64;
    let text = basemap::ATTRIBUTION.join(" · ");
    let tw = draw::text_length(FONT, fs, &text).ok()?;
    let sz = size as f64;
    im.rect(sz - tw - 12.0, sz - fs - 10.0, sz, sz, [10.0, 12.0, 16.0], 1.0);
    draw::draw_text(&mut im, sz - tw - 6.0, sz - fs - 7.0, &text, FONT, fs, [200.0, 204.0, 210.0], 1.0).ok()?;
    Some(im)
}

/// Image W×H de la carte de fin → `path` ; `credits` : lignes (musique) en bas de l'image.
pub fn render(results: &[&Analysis], w: usize, h: usize, path: &Path, title: &str, credits: &[String]) -> Result<Summary> {
    let s = summary(results);
    let mut im = Canvas::filled(w, h, [BG[0], BG[1], BG[2], 255.0]);
    let (wf, hf) = (w as f64, h as f64);
    let u = wf.min(hf);
    let landscape = wf > hf * 1.2;   // 16:9 : carte à gauche, chiffres à droite ; carré et vertical : l'un sous l'autre
    let square = !landscape && hf < wf * 1.2;
    let msize = if landscape { (hf * 0.78) as usize } else {
        (wf * if square { if credits.is_empty() { 0.52 } else { 0.42 } } else { 0.82 }) as usize
    };
    let k = if square { 0.8 } else if landscape { 1.0 } else { 1.2 };   // taille du texte selon la place disponible
    let mp = map(results, msize);
    let (mx, my, tx, ty);
    if landscape {
        (mx, my) = ((wf * 0.06) as i64, (h as i64 - msize as i64).div_euclid(2));
        (tx, ty) = (mx + msize as i64 + (wf * 0.05) as i64, (hf * 0.2) as i64);
    } else {
        (mx, my) = ((w as i64 - msize as i64).div_euclid(2), (hf * if square { 0.05 } else { 0.1 }) as i64);
        tx = if !square { (w as i64 - msize as i64).div_euclid(2) } else { (wf * 0.24) as i64 };
        ty = my + msize as i64 + (u * 0.05) as i64;
    }
    if let Some(mut mp) = mp {
        // coins arrondis : masque rounded_rectangle((0, 0, msize − 1, msize − 1), radius = msize // 25)
        let radius = (msize / 25) as f64;
        for y in 0..msize {
            for x in 0..msize {
                mp.px[y * msize + x][3] = (draw::rounded_coverage(msize, msize, radius, x as f64 + 0.5, y as f64 + 0.5) * 255.0) as f32;
            }
        }
        im.over(&mp, mx, my);
    }
    let fsize = |r: f64| ((u * r * k) as usize) as f64;
    let (big, mid, small) = (fsize(0.075), fsize(0.052), fsize(0.034));
    let tx = tx as f64;
    let mut y = ty as f64;
    if !title.is_empty() {
        draw::draw_text(&mut im, tx, y, title, FONT_BOLD, big, [255.0; 3], 1.0)?;
        y += (u * 0.1 * k) as i64 as f64;
    }
    draw::draw_text(&mut im, tx, y, &date_label(&s.days), FONT, small, [180.0, 186.0, 196.0], 1.0)?;
    let step = (u * 0.075 * k) as i64 as f64;
    y += step;
    let minutes = (s.moving_s as i64).div_euclid(60);
    let (hh, mm) = (minutes.div_euclid(60), minutes.rem_euclid(60));
    let mut lines: Vec<(String, &str)> = vec![];
    if s.distance_km != 0.0 {
        lines.push((format!("{:.0} km", s.distance_km).replace('.', ","), "parcourus"));
    }
    if s.moving_s != 0.0 {
        lines.push((if hh != 0 { format!("{hh} h {mm:02}") } else { format!("{mm} min") }, "de roulage"));
    }
    if s.climb_m != 0.0 {
        lines.push((format!("{} m", num(s.climb_m)), "de dénivelé positif"));
    }
    if let Some(a) = s.alt_max_m.filter(|a| *a != 0.0) {
        lines.push((format!("{} m", num(a)), "point culminant"));
    }
    if let Some(v) = s.max_speed_kmh.filter(|v| *v != 0.0) {
        lines.push((format!("{v:.0} km/h"), "vitesse max"));
    }
    for (value, label) in &lines {
        draw::draw_text(&mut im, tx, y, value, FONT_BOLD, mid, ACCENT, 1.0)?;
        let vw = draw::text_length(FONT_BOLD, mid, &format!("{value}  "))?;
        draw::draw_text(&mut im, tx + vw, y + (u * 0.016 * k) as i64 as f64, label, FONT, small, [220.0, 224.0, 230.0], 1.0)?;
        y += step;
    }
    if !credits.is_empty() {   // crédit de la musique (licence CC BY) : sous les chiffres, discret mais lisible
        let mut size = (u * 0.024 * k) as i64;
        let mut cf = size.max(12) as f64;
        let room = wf - tx - (u * 0.04) as i64 as f64;
        let widest = |f: f64| -> Result<f64> {
            credits.iter().map(|l| draw::text_length(FONT, f, l)).try_fold(f64::NEG_INFINITY, |m, x| Ok(m.max(x?)))
        };
        while size > 12 && widest(cf)? > room {   // tient dans la colonne
            size -= 1;
            cf = size as f64;
        }
        y += (u * 0.05 * k) as i64 as f64;
        for line in credits {
            draw::draw_text(&mut im, tx, y, line, FONT, cf, [160.0, 166.0, 176.0], 1.0)?;
            y += (size as f64 * 1.45) as i64 as f64;
        }
    }
    let rgba = im.to_rgba8();
    image::DynamicImage::ImageRgba8(rgba).to_rgb8().save(path)?;
    Ok(s)
}
