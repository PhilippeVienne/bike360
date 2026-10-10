//! Analyse d'arrivée d'un dossier de rushs, pour le service hébergé : analyse des sessions, une
//! vignette par session et un résumé que la Bibliothèque affiche sans ouvrir l'atelier.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::analyze::{self, Analysis};
use crate::insta360::{self, Session};
use crate::position;

/// Champ des deux objectifs de la caméra (°), comme pour l'horizon et les exports.
const LENS_FOV: f64 = 195.0;
const THUMB_WIDTH: u32 = 480;
const THUMB_FOV: f64 = 100.0;
const THUMB_PITCH: f64 = -10.0;

/// Vignette de la session : vue avant au tiers de sa durée, tirée de l'aperçu.
pub fn thumbnail(session: &Session, result: &Analysis, yaw: f64, out: &Path) -> Result<()> {
    let t = result.duration as f64 / 3.0;
    let (seg, info) = session.segments.iter().zip(&result.segments)
        .find(|(_, i)| i.offset <= t && t < i.offset + i.duration)
        .or_else(|| session.segments.iter().zip(&result.segments).next_back())
        .context("aucun segment")?;
    let lrv = seg.lrv.as_ref().context("pas de .lrv")?;
    let local = (t - info.offset).min(info.duration - 0.5).max(0.0);
    let height = THUMB_WIDTH * 9 / 16;
    let v_fov = 2.0 * ((THUMB_FOV.to_radians() / 2.0).tan() * height as f64 / THUMB_WIDTH as f64).atan().to_degrees();
    let vf = format!("v360=input=dfisheye:ih_fov={LENS_FOV:.0}:iv_fov={LENS_FOV:.0}:output=flat:yaw={yaw:.1}:pitch={THUMB_PITCH:.1}\
                      :h_fov={THUMB_FOV:.1}:v_fov={v_fov:.2}:w={THUMB_WIDTH}:h={height}");
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-ss", &format!("{local:.2}"), "-i"]).arg(lrv)
        .args(["-frames:v", "1", "-vf", &vf, "-q:v", "5"]).arg(out)
        .status()?;
    if !st.success() {
        bail!("ffmpeg a échoué (vignette)");
    }
    Ok(())
}

/// Analyse les sessions de `rushs` et écrit une vignette par session dans `out`.
/// Résumé par session : identifiant, durée, GPS, moments forts, statistiques, caméra, vignette.
pub fn run(rushs: &Path, out: &Path) -> Result<Vec<Value>> {
    std::fs::create_dir_all(out)?;
    let sessions = insta360::scan(rushs);
    let durations: std::collections::HashMap<String, f64> = sessions.iter()
        .map(|s| (s.id.clone(), s.segments.iter().filter_map(|x| x.lrv.as_ref()).map(|p| analyze::file_duration(p)).sum()))
        .collect();
    let blocks = insta360::merge_continuous(sessions, |s| durations[&s.id]);
    let mut summary = vec![];
    for (s, r) in analyze::analyze_sessions(blocks, false)? {
        let thumb = out.join(format!("{}.jpg", s.id));
        let thumb_ok = thumbnail(&s, &r, position::front_yaw(position::DEFAULT), &thumb)
            .inspect_err(|e| eprintln!("vignette de {} : {e:#}", s.id)).is_ok();
        summary.push(json!({
            "id": r.id, "date": r.date, "time": r.time, "duration_s": r.duration, "gps_coverage": r.gps_coverage,
            "gps_source": r.gps_source, "candidates": r.candidates.len(), "stats": r.stats, "camera": s.camera,
            "parts": s.parts, "thumb": thumb_ok.then(|| thumb.file_name().map(|n| n.to_string_lossy().to_string())),
        }));
    }
    Ok(summary)
}
