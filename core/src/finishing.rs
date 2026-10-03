//! Finition d'un montage : transitions entre clips, titre d'ouverture, pistes audio.
//!
//! Chaque clip arrive en un fichier (vidéo + son) ; ffmpeg les enchaîne (xfade/acrossfade, ou
//! concat sans transition), ajoute fondus d'ouverture/fermeture, titre et musique, puis réencode
//! le tout une seule fois.
//!
//! API (le style est un objet JSON, comme dans le projet) :
//! - [`TRANSITIONS`], [`defaults`]`() -> Value`, [`END_CARD_S`].
//! - [`clean`]`(style: Option<&Value>) -> Result<Value>` : style validé (clés connues, bornes) ;
//!   erreur si une valeur numérique n'est pas convertible (comme float() en Python).
//!   Les pistes audio sont dans `audio_tracks` ; l'ancienne clé `music` (une seule musique)
//!   est convertie en piste à la lecture.
//! - [`track_files`]`(style) -> Vec<String>` : fichiers audio utilisés par les pistes.
//! - [`Track`] : piste audio résolue (fichier sur disque + durée du fichier).
//! - [`is_plain`]`(style) -> bool` : simple assemblage sans réencodage.
//! - [`probe`]`(path) -> Result<(durée, a_du_son)>`.
//! - [`finish_command`]`(clip_files, info, final, style, encoder_args, W, H, tracks, audio_bitrate,
//!   end_card, credits) -> (arguments ffmpeg, durée totale)` : construction pure (testable).
//! - [`finish`]`(clip_files, final, style, encoder_args, W, H, tracks, audio_bitrate, end_card,
//!   credits) -> Result<f64>` : sonde les clips, lance ffmpeg, renvoie la durée totale.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};

use crate::draw::{FONT, FONT_BOLD};
use crate::telemetry::{escape, run, truthy};

/// Nom affiché → transition xfade (None : coupe franche).
pub const TRANSITIONS: [(&str, Option<&str>); 4] =
    [("aucune", None), ("fondu", Some("fade")), ("noir", Some("fadeblack")), ("glisse", Some("slideleft"))];
pub const END_CARD_S: f64 = 5.0;

/// Style par défaut (même ordre de clés que la version Python).
pub fn defaults() -> Value {
    json!({"transition": "fondu", "duration": 0.6, "title": "", "subtitle": "",
           "audio_tracks": [], "original_volume": 1.0, "end_card": true})
}

const KEYS: [&str; 7] = ["transition", "duration", "title", "subtitle", "audio_tracks", "original_volume", "end_card"];
/// Pistes audio au plus dans un montage.
pub const MAX_TRACKS: usize = 8;

/// Piste audio prête pour ffmpeg : le fichier, sa durée, et les réglages de la piste
/// (objet de `audio_tracks` : start, offset, length, volume, fade_in, fade_out, loop).
pub struct Track<'a> {
    pub path: PathBuf,
    pub seconds: f64,
    pub spec: &'a Value,
}

fn transition(name: &str) -> Option<Option<&'static str>> {
    TRANSITIONS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// float() Python d'une valeur JSON.
fn py_float(v: &Value) -> Result<f64> {
    match v {
        Value::Number(n) => n.as_f64().context("nombre"),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::String(s) => {
            let t = s.trim().replace('_', "");
            t.parse::<f64>().or_else(|_| match t.to_ascii_lowercase().trim_start_matches(['+', '-']) {
                "infinity" | "inf" => Ok(if t.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }),
                "nan" => Ok(f64::NAN),
                _ => Err(anyhow!("could not convert string to float: {s:?}")),
            })
        }
        _ => Err(anyhow!("float() argument must be a string or a real number, not {v}")),
    }
}

/// str() Python d'une valeur JSON (cas courants).
fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::Number(n) if n.is_f64() => {
            let x = n.as_f64().unwrap();
            if x.fract() == 0.0 && x.abs() < 1e16 { format!("{x:.1}") } else { format!("{x}") }
        }
        other => other.to_string(),
    }
}

/// max(lo, min(hi, x)) avec la sémantique Python (NaN → hi).
fn bound(x: f64, lo: f64, hi: f64) -> f64 {
    let m = if x < hi { x } else { hi };
    if m > lo { m } else { lo }
}

/// Style validé (valeurs bornées, clés connues).
pub fn clean(style: Option<&Value>) -> Result<Value> {
    let mut s: Map<String, Value> = defaults().as_object().unwrap().clone();
    if let Some(Value::Object(o)) = style {
        for (k, v) in o {
            if KEYS.contains(&k.as_str()) {
                s.insert(k.clone(), v.clone());
            }
        }
        // ancien style : une seule musique (`music`, `music_volume`) → une piste qui boucle sur tout le montage
        let legacy = o.get("music").map(py_str).unwrap_or_default();
        if !o.contains_key("audio_tracks") && !legacy.is_empty() {
            let v = o.get("music_volume").map_or(Ok(0.8), py_float)?;
            s.insert("audio_tracks".into(), json!([{"id": "a1", "file": legacy, "volume": v, "loop": true}]));
        }
    }
    let t = s["transition"].as_str().filter(|t| transition(t).is_some()).unwrap_or("fondu").to_string();
    s.insert("transition".into(), t.into());
    s.insert("duration".into(), bound(py_float(&s["duration"])?, 0.2, 2.0).into());
    s.insert("original_volume".into(), bound(py_float(&s["original_volume"])?, 0.0, 1.5).into());
    let tracks = clean_tracks(&s["audio_tracks"])?;
    s.insert("audio_tracks".into(), tracks);
    for k in ["title", "subtitle"] {
        let v: String = py_str(&s[k]).chars().take(120).collect();
        s.insert(k.into(), v.into());
    }
    let e = truthy(&s["end_card"]);
    s.insert("end_card".into(), e.into());
    Ok(Value::Object(s))
}

/// Pistes audio validées : au plus MAX_TRACKS, fichier obligatoire, valeurs bornées.
///
/// Réglages d'une piste : `id`, `file`, `start` (s, dans le montage), `offset` (s, début dans le
/// fichier), `length` (s jouées, null = jusqu'à la fin du fichier ou du montage), `volume`,
/// `fade_in`, `fade_out` (s), `loop` (le fichier se répète, alors `offset` vaut 0), `muted`.
fn clean_tracks(v: &Value) -> Result<Value> {
    let mut out: Vec<Value> = vec![];
    for (i, t) in v.as_array().into_iter().flatten().enumerate() {
        let Some(o) = t.as_object() else { continue };
        let file: String = o.get("file").map(py_str).unwrap_or_default().chars().take(120).collect();
        if file.is_empty() || file == "None" || out.len() >= MAX_TRACKS {
            continue;
        }
        let f = |k: &str, d: f64, lo: f64, hi: f64| -> Result<f64> {
            Ok(bound(o.get(k).filter(|v| !v.is_null()).map_or(Ok(d), py_float)?, lo, hi))
        };
        let looped = o.get("loop").is_some_and(truthy);
        let id: String = o.get("id").map(py_str).filter(|s| !s.is_empty() && s != "None")
            .unwrap_or_else(|| format!("a{}", i + 1)).chars().take(16).collect();
        let length = match o.get("length") {
            None | Some(Value::Null) => Value::Null,
            Some(l) => bound(py_float(l)?, 0.1, 36000.0).into(),
        };
        out.push(json!({"id": id, "file": file, "start": f("start", 0.0, 0.0, 36000.0)?,
                        "offset": if looped { 0.0 } else { f("offset", 0.0, 0.0, 36000.0)? },
                        "length": length, "volume": f("volume", 0.8, 0.0, 1.5)?,
                        "fade_in": f("fade_in", 1.5, 0.0, 30.0)?, "fade_out": f("fade_out", 3.0, 0.0, 30.0)?,
                        "loop": looped, "muted": o.get("muted").is_some_and(truthy)}));
    }
    Ok(Value::Array(out))
}

/// Fichiers audio utilisés par les pistes du style (sans doublon, dans l'ordre).
pub fn track_files(style: &Value) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for t in style["audio_tracks"].as_array().into_iter().flatten() {
        if let Some(f) = t["file"].as_str().filter(|f| !out.iter().any(|o| o == f)) {
            out.push(f.to_string());
        }
    }
    out
}

fn text<'a>(style: &'a Value, k: &str) -> &'a str {
    style.get(k).and_then(Value::as_str).unwrap_or_default()
}

fn num(style: &Value, k: &str) -> f64 {
    style.get(k).and_then(Value::as_f64).unwrap_or_else(|| defaults()[k].as_f64().unwrap())
}

fn kind(style: &Value) -> Option<&'static str> {
    transition(text(style, "transition")).flatten()
}

/// Rien à faire au-delà d'un simple assemblage (copie sans réencodage).
pub fn is_plain(style: &Value) -> bool {
    kind(style).is_none() && text(style, "title").is_empty() && !style.get("end_card").is_some_and(truthy)
        && style["audio_tracks"].as_array().is_none_or(Vec::is_empty) && (num(style, "original_volume") - 1.0).abs() < 0.005
}

/// Durée et présence d'une piste son (ffprobe).
pub fn probe(path: &Path) -> Result<(f64, bool)> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type", "-of", "json"])
        .arg(path)
        .output()?;
    let out: Value = serde_json::from_slice(&o.stdout).with_context(|| format!("ffprobe {path:?}"))?;
    let d = out["format"]["duration"].as_str().and_then(|s| s.parse().ok()).with_context(|| format!("durée de {path:?}"))?;
    let audio = out["streams"].as_array().is_some_and(|s| s.iter().any(|x| x["codec_type"] == "audio"));
    Ok((d, audio))
}

/// Commande ffmpeg de finition et durée totale, à partir des (durée, son) de chaque clip.
#[allow(clippy::too_many_arguments)]
pub fn finish_command(clip_files: &[&Path], clip_info: &[(f64, bool)], final_: &Path, style: &Value, encoder_args: &[String],
                      w: usize, h: usize, tracks: &[Track], audio_bitrate: &str, end_card: Option<&Path>,
                      credits: &[String]) -> (Vec<String>, f64) {
    let mut info = clip_info.to_vec();
    if end_card.is_some() {
        info.push((END_CARD_S, false));
    }
    let n = info.len();
    let mut kind = kind(style);
    if end_card.is_some() && kind.is_none() {   // coupe franche entre les clips, mais fondu vers la carte de fin
        kind = Some("fade");
    }
    let dmin = info.iter().map(|x| x.0).fold(f64::INFINITY, f64::min);
    let t = if kind.is_some() && n > 1 { num(style, "duration").min(dmin / 2.0) } else { 0.0 };
    let mut inputs: Vec<String> = vec![];
    let mut graph: Vec<String> = vec![];
    let files: Vec<&Path> = clip_files.iter().copied().chain(end_card).collect();
    for (k, (f, &(d, has_audio))) in files.iter().zip(&info).enumerate() {
        if end_card.is_some() && k == n - 1 {
            inputs.extend(["-loop", "1", "-framerate", "30000/1001", "-t"].map(String::from));
            inputs.push(format!("{d:.3}"));
        }
        inputs.push("-i".into());
        inputs.push(f.to_string_lossy().into_owned());
        graph.push(format!("[{k}:v]fps=30000/1001,format=yuv420p,setsar=1,settb=AVTB[v{k}]"));
        if has_audio {
            graph.push(format!("[{k}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo[a{k}]"));
        } else {   // clip sans son : silence de même durée, pour garder l'enchaînement audio
            graph.push(format!("anullsrc=r=48000:cl=stereo,atrim=0:{d:.3},aformat=sample_fmts=fltp[a{k}]"));
        }
    }

    let (mut v, mut a, total);
    if t > 0.0 {
        let kind = kind.unwrap();
        let mut acc = info[0].0;
        (v, a) = ("v0".to_string(), "a0".to_string());
        for k in 1..n {
            graph.push(format!("[{v}][v{k}]xfade=transition={kind}:duration={t:.3}:offset={:.3}[x{k}]", acc - t));
            graph.push(format!("[{a}][a{k}]acrossfade=d={t:.3}[y{k}]"));
            (v, a, acc) = (format!("x{k}"), format!("y{k}"), acc + info[k].0 - t);
        }
        total = acc;
    } else {
        graph.push((0..n).map(|k| format!("[v{k}][a{k}]")).collect::<String>() + &format!("concat=n={n}:v=1:a=1[xc][yc]"));
        (v, a, total) = ("xc".to_string(), "yc".to_string(), info.iter().map(|x| x.0).sum());
    }

    // ouverture au noir, fermeture au noir (image et son)
    let (fin, fout) = (0.8, 1.5f64.min(total / 4.0));
    graph.push(format!("[{v}]fade=t=in:st=0:d={fin},fade=t=out:st={:.3}:d={fout:.3}[vf]", total - fout));
    graph.push(format!("[{a}]afade=t=in:st=0:d={fin},afade=t=out:st={:.3}:d={fout:.3}[af]", total - fout));
    (v, a) = ("vf".into(), "af".into());

    let u = w.min(h) as f64;
    let title = text(style, "title");
    if !title.is_empty() {
        let alpha = "if(lt(t,0.8),0,if(lt(t,1.6),(t-0.8)/0.8,if(lt(t,4.4),1,max(0,(5.2-t)/0.8))))";
        let mut lines = vec![(title, FONT_BOLD, (u * 0.085) as usize, 0.0)];
        let subtitle = text(style, "subtitle");
        if !subtitle.is_empty() {
            lines.push((subtitle, FONT, (u * 0.042) as usize, u * 0.075));
        }
        for (k, (text, font, size, dy)) in lines.into_iter().enumerate() {
            graph.push(format!("[{v}]drawtext=fontfile={font}:text='{}':fontsize={size}:fontcolor=white\
                                :alpha='{alpha}':shadowcolor=black@0.55:shadowx=3:shadowy=3\
                                :x=(w-tw)/2:y=h*0.42-th/2+{dy:.0}:enable='lt(t,5.3)'[t{k}]", escape(text)));
            v = format!("t{k}");
        }
    }

    if !credits.is_empty() && end_card.is_none() {   // crédit de la musique incrusté sur les dernières secondes
        let (fs, lh) = (14.max((u * 0.026) as usize), (u * 0.036) as i64);
        for (k, line) in credits.iter().enumerate() {
            let y = h as i64 - (u * 0.05) as i64 - lh * (credits.len() - k) as i64;
            graph.push(format!("[{v}]drawtext=fontfile={FONT}:text='{}':fontsize={fs}:fontcolor=white\
                                :shadowcolor=black@0.7:shadowx=2:shadowy=2:x=(w-tw)/2:y={y}\
                                :enable='gte(t,{:.2})'[c{k}]", escape(line), 0f64.max(total - 6.0)));
            v = format!("c{k}");
        }
    }

    // pistes audio : chacune recadrée (début dans le fichier, durée), fondue, réglée en volume et
    // décalée à sa place dans le montage, puis mixée avec le son d'origine
    let mut mix: Vec<String> = vec![];
    for tr in tracks {
        let s = tr.spec;
        let g = |k: &str, d: f64| s.get(k).and_then(Value::as_f64).unwrap_or(d);
        let (start, offset, volume, looped) = (g("start", 0.0), g("offset", 0.0), g("volume", 0.8), truthy(&s["loop"]));
        if volume <= 0.0 || truthy(&s["muted"]) || start >= total - 0.05 {
            continue;
        }
        let avail = if looped { f64::INFINITY } else { (tr.seconds - offset).max(0.0) };
        let len = s.get("length").and_then(Value::as_f64).unwrap_or(f64::INFINITY).min(avail).min(total - start);
        if len < 0.05 {
            continue;
        }
        let m = inputs.iter().filter(|x| *x == "-i").count();
        if looped {
            inputs.extend(["-stream_loop", "-1"].map(String::from));
        }
        inputs.extend(["-i".to_string(), tr.path.to_string_lossy().into_owned()]);
        let (fi, fo) = (g("fade_in", 1.5).min(len / 2.0), g("fade_out", 3.0).min(len / 2.0));
        let ms = (start * 1000.0).round() as i64;
        let k = mix.len();
        graph.push(format!("[{m}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,\
                            atrim=start={offset:.3}:duration={len:.3},asetpts=PTS-STARTPTS,\
                            afade=t=in:st=0:d={fi:.3},afade=t=out:st={:.3}:d={fo:.3},volume={volume:.3},\
                            adelay={ms}|{ms}[mus{k}]", (len - fo).max(0.0)));
        mix.push(format!("[mus{k}]"));
    }
    let orig = num(style, "original_volume");
    if !mix.is_empty() || (orig - 1.0).abs() >= 0.005 {
        graph.push(format!("[{a}]volume={orig:.3}[orig]"));
        a = "orig".into();
    }
    if !mix.is_empty() {
        graph.push(format!("[orig]{}amix=inputs={}:duration=first:normalize=0[am]", mix.concat(), mix.len() + 1));
        a = "am".into();
    }

    let mut cmd: Vec<String> = ["ffmpeg", "-v", "error", "-y"].map(String::from).to_vec();
    cmd.extend(inputs);
    cmd.extend(["-filter_complex".to_string(), graph.join(";"), "-map".into(), format!("[{v}]"), "-map".into(), format!("[{a}]")]);
    cmd.extend(encoder_args.iter().cloned());
    cmd.extend(["-c:a".to_string(), "aac".into(), "-b:a".into(), audio_bitrate.into(), "-movflags".into(), "+faststart".into()]);
    cmd.push(final_.to_string_lossy().into_owned());
    (cmd, total)
}

/// Assemble les clips (dans l'ordre) avec transitions, titre et musique → `final_`.
///
/// `end_card` : image de fin (PNG W×H) affichée END_CARD_S secondes après le dernier clip ;
/// `credits` : lignes de crédit de la musique (incrustées à la fin s'il n'y a pas de carte de fin).
#[allow(clippy::too_many_arguments)]
pub fn finish(clip_files: &[&Path], final_: &Path, style: &Value, encoder_args: &[String], w: usize, h: usize,
              tracks: &[Track], audio_bitrate: &str, end_card: Option<&Path>, credits: &[String]) -> Result<f64> {
    let info = clip_files.iter().map(|f| probe(f)).collect::<Result<Vec<_>>>()?;
    let (cmd, total) = finish_command(clip_files, &info, final_, style, encoder_args, w, h, tracks, audio_bitrate,
                                      end_card, credits);
    run(&cmd)?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_music_becomes_a_looping_track() {
        let s = clean(Some(&json!({"music": "a.mp3", "music_volume": 0.5}))).unwrap();
        let t = &s["audio_tracks"][0];
        assert_eq!((t["file"].as_str(), t["volume"].as_f64(), t["loop"].as_bool()), (Some("a.mp3"), Some(0.5), Some(true)));
        assert!(s.get("music").is_none());
        // un style qui a déjà ses pistes ignore l'ancienne clé
        let s = clean(Some(&json!({"music": "a.mp3", "audio_tracks": []}))).unwrap();
        assert!(s["audio_tracks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn tracks_are_bounded() {
        let many: Vec<Value> = (0..12).map(|i| json!({"file": format!("{i}.mp3"), "volume": 9, "offset": 3, "loop": true})).collect();
        let s = clean(Some(&json!({"audio_tracks": many.into_iter().chain([json!({"file": ""})]).collect::<Vec<_>>()}))).unwrap();
        let t = s["audio_tracks"].as_array().unwrap();
        assert_eq!(t.len(), MAX_TRACKS);
        assert_eq!((t[0]["volume"].as_f64(), t[0]["offset"].as_f64()), (Some(1.5), Some(0.0)));
        assert_eq!(track_files(&s).len(), MAX_TRACKS);
    }

    #[test]
    fn mix_places_each_track() {
        let style = clean(Some(&json!({"transition": "aucune", "end_card": false, "original_volume": 0.4, "audio_tracks": [
            {"file": "a.mp3", "start": 2.0, "offset": 1.0, "length": 5.0, "volume": 0.7, "fade_in": 0.5, "fade_out": 1.0},
            {"file": "b.mp3", "start": 8.0, "loop": true},
            {"file": "c.mp3", "muted": true}]}))).unwrap();
        let tracks: Vec<Track> = style["audio_tracks"].as_array().unwrap().iter()
            .map(|t| Track { path: PathBuf::from(t["file"].as_str().unwrap()), seconds: 30.0, spec: t }).collect();
        let clips = [Path::new("c1.mp4"), Path::new("c2.mp4")];
        let (cmd, total) = finish_command(&clips, &[(6.0, true), (6.0, true)], Path::new("o.mp4"), &style, &[], 1920, 1080,
                                          &tracks, "160k", None, &[]);
        assert_eq!(total, 12.0);
        let g = &cmd[cmd.iter().position(|x| x == "-filter_complex").unwrap() + 1];
        assert!(g.contains("atrim=start=1.000:duration=5.000"), "{g}");
        assert!(g.contains("adelay=2000|2000[mus0]") && g.contains("adelay=8000|8000[mus1]"), "{g}");
        assert!(g.contains("atrim=start=0.000:duration=4.000"), "piste en boucle bornée au montage : {g}");
        assert!(g.contains("[orig][mus0][mus1]amix=inputs=3"), "piste muette exclue : {g}");
        assert_eq!(cmd.iter().filter(|x| *x == "-stream_loop").count(), 1);
        assert_eq!(cmd.iter().filter(|x| *x == "-i").count(), 4);
    }
}
