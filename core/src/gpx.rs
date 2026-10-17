//! Traces GPX déposées dans data/gps/ : une source de positions autre que GeoRide (application de
//! téléphone, GPS de guidon, traceur qui sait exporter). Lecture sans dépendance XML : seuls les
//! points de trace (`<trkpt>`) et leurs champs usuels (altitude, heure, vitesse, cap) sont lus.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use chrono::{DateTime, Duration, NaiveDate};
use regex::Regex;

use crate::analyze::Gps;
use crate::numeric::unwrap;
use crate::paths;

const EARTH_M: f64 = 6371000.0;
const MS_TO_KMH: f64 = 3.6;
/// En dessous de ce déplacement entre deux points, le cap n'est pas mesurable : on garde le précédent.
const MIN_MOVE_M: f64 = 1.0;
/// Marge après la fin du jour UTC (une balade commencée le soir déborde sur le lendemain).
const DAY_MARGIN_H: i64 = 6;

/// Dossier des traces déposées.
pub fn dir() -> PathBuf {
    paths::data().join("gps")
}

/// Point de trace : temps UTC (s), position, altitude (m), vitesse (km/h) et cap (°) s'ils sont fournis.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub t: f64,
    pub lat: f64,
    pub lon: f64,
    pub alt: f64,
    pub speed: Option<f64>,
    pub heading: Option<f64>,
}

struct Patterns {
    trkpt: Regex,
    lat: Regex,
    lon: Regex,
    ele: Regex,
    time: Regex,
    speed: Regex,
    course: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    // les champs peuvent porter un préfixe d'extension (gpxtpx:speed, osmand:speed…)
    let tag = |name: &str| Regex::new(&format!(r"<(?:\w+:)?{name}>\s*([^<]+?)\s*<")).unwrap();
    let attr = |name: &str| Regex::new(&format!(r#"\b{name}\s*=\s*["']([^"']+)["']"#)).unwrap();
    P.get_or_init(|| Patterns {
        trkpt: Regex::new(r"(?s)<trkpt\b([^>]*)>(.*?)</trkpt>").unwrap(),
        lat: attr("lat"),
        lon: attr("lon"),
        ele: tag("ele"),
        time: tag("time"),
        speed: tag("speed"),
        course: tag("course"),
    })
}

/// Points horodatés d'un fichier GPX, triés par temps (les points sans heure ou sans position sont ignorés).
pub fn parse(text: &str) -> Vec<Point> {
    let p = patterns();
    let num = |re: &Regex, s: &str| re.captures(s).and_then(|m| m[1].parse::<f64>().ok());
    let mut out: Vec<Point> = p.trkpt.captures_iter(text)
        .filter_map(|m| {
            let (attrs, body) = (&m[1], &m[2]);
            let t = p.time.captures(body).and_then(|m| DateTime::parse_from_rfc3339(&m[1]).ok())?;
            Some(Point {
                t: t.timestamp_micros() as f64 / 1e6,
                lat: num(&p.lat, attrs)?,
                lon: num(&p.lon, attrs)?,
                alt: num(&p.ele, body).unwrap_or(0.0),
                speed: num(&p.speed, body).map(|v| v * MS_TO_KMH),   // m/s dans GPX
                heading: num(&p.course, body),
            })
        })
        .collect();
    out.sort_by(|a, b| a.t.total_cmp(&b.t));
    out.dedup_by(|b, a| b.t <= a.t);
    out
}

/// Distance (m) et cap (°, 0 = nord, sens horaire) de `a` vers `b`.
fn leg(a: &Point, b: &Point) -> (f64, f64) {
    let (la0, la1, dl) = (a.lat.to_radians(), b.lat.to_radians(), (b.lon - a.lon).to_radians());
    let h = ((la1 - la0) / 2.0).sin().powi(2) + la0.cos() * la1.cos() * (dl / 2.0).sin().powi(2);
    let bearing = (dl.sin() * la1.cos()).atan2(la0.cos() * la1.sin() - la0.sin() * la1.cos() * dl.cos());
    (2.0 * EARTH_M * h.sqrt().asin(), bearing.to_degrees().rem_euclid(360.0))
}

/// Série de positions pour l'analyse ; vitesse et cap déduits des positions voisines quand le
/// fichier ne les donne pas. None avec moins de deux points.
pub fn to_gps(points: &[Point]) -> Option<Gps> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    let (mut speed, mut heading) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for (i, p) in points.iter().enumerate() {
        let (a, b) = (&points[i.saturating_sub(1)], &points[(i + 1).min(n - 1)]);
        let (dist, bearing) = leg(a, b);
        speed.push(p.speed.unwrap_or(dist / (b.t - a.t) * MS_TO_KMH));
        heading.push(match p.heading {
            Some(h) => h,
            None if dist >= MIN_MOVE_M || i == 0 => bearing,
            None => *heading.last().unwrap(),
        });
    }
    Some(Gps {
        t: points.iter().map(|p| p.t).collect(),
        lat: points.iter().map(|p| p.lat).collect(),
        lon: points.iter().map(|p| p.lon).collect(),
        speed,
        alt: points.iter().map(|p| p.alt).collect(),
        heading: unwrap(&heading.iter().map(|v| v.to_radians()).collect::<Vec<_>>())
            .iter().map(|v| v.to_degrees()).collect(),
    })
}

/// Points d'un fichier, relus seulement s'il a changé (taille ou date).
fn points_of(path: &Path) -> Arc<Vec<Point>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (u64, Option<SystemTime>, Arc<Vec<Point>>)>>> = OnceLock::new();
    let meta = std::fs::metadata(path).ok();
    let stamp = (meta.as_ref().map_or(0, |m| m.len()), meta.and_then(|m| m.modified().ok()));
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some((len, modified, pts)) = cache.get(path) {
        if (*len, *modified) == stamp {
            return pts.clone();
        }
    }
    let pts = Arc::new(std::fs::read_to_string(path).map(|t| parse(&t)).unwrap_or_default());
    cache.insert(path.to_path_buf(), (stamp.0, stamp.1, pts.clone()));
    pts
}

/// Fichiers .gpx déposés, par ordre de nom.
pub fn files() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir()).into_iter().flatten().flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("gpx")))
        .collect();
    out.sort();
    out
}

/// Points de tous les fichiers déposés qui tombent dans le jour UTC `day` (avec une marge après minuit).
pub fn load_day(day: NaiveDate) -> Option<Gps> {
    let start = day.and_hms_opt(0, 0, 0)?.and_utc();
    let (t0, t1) = (start.timestamp() as f64, (start + Duration::hours(24 + DAY_MARGIN_H)).timestamp() as f64);
    let mut pts: Vec<Point> = files().iter()
        .flat_map(|f| points_of(f).iter().filter(|p| p.t >= t0 && p.t < t1).cloned().collect::<Vec<_>>())
        .collect();
    pts.sort_by(|a, b| a.t.total_cmp(&b.t));
    pts.dedup_by(|b, a| b.t <= a.t);
    to_gps(&pts)
}

/// Résumé d'un fichier déposé : (nombre de points, premier et dernier temps UTC en secondes).
pub fn summary(path: &Path) -> (usize, Option<(f64, f64)>) {
    let pts = points_of(path);
    (pts.len(), pts.first().zip(pts.last()).map(|(a, b)| (a.t, b.t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<gpx version="1.1" xmlns:gpxtpx="http://www.garmin.com/xmlschemas/TrackPointExtension/v1"><trk><trkseg>
<trkpt lat="45.0000" lon="5.0000"><ele>200</ele><time>2026-09-20T07:00:00Z</time></trkpt>
<trkpt lon="5.0000" lat="45.0001"><ele>201.5</ele><time>2026-09-20T07:00:01Z</time></trkpt>
<trkpt lat="45.0002" lon="5.0000"><time>2026-09-20T07:00:02.000+00:00</time>
  <extensions><gpxtpx:TrackPointExtension><gpxtpx:speed>10</gpxtpx:speed><gpxtpx:course>90</gpxtpx:course></gpxtpx:TrackPointExtension></extensions></trkpt>
<trkpt lat="45.0003" lon="5.0000"><ele>203</ele></trkpt>
<trkpt lat="45.0002" lon="5.0000"><time>2026-09-20T07:00:02Z</time></trkpt>
</trkseg></trk></gpx>"#;

    #[test]
    fn reads_track_points() {
        let pts = parse(SAMPLE);
        assert_eq!(pts.len(), 3, "le point sans heure et le doublon sont écartés");
        assert_eq!((pts[1].lat, pts[1].lon, pts[1].alt), (45.0001, 5.0, 201.5));
        assert_eq!(pts[1].t - pts[0].t, 1.0);
        assert_eq!((pts[2].speed, pts[2].heading), (Some(36.0), Some(90.0)));
        assert_eq!(pts[0].speed, None);
    }

    #[test]
    fn derives_speed_and_heading() {
        let g = to_gps(&parse(SAMPLE)).unwrap();
        // 0,0001° de latitude ≈ 11,1 m par seconde vers le nord, soit ≈ 40 km/h
        assert!((g.speed[0] - 40.0).abs() < 0.5 && (g.speed[1] - 40.0).abs() < 0.5, "{:?}", g.speed);
        assert!(g.heading[0].abs() < 0.01 && g.heading[1].abs() < 0.01, "{:?}", g.heading);
        assert_eq!((g.speed[2], g.heading[2]), (36.0, 90.0), "valeurs du fichier gardées");
        assert!(to_gps(&parse(SAMPLE)[..1]).is_none());
    }
}
