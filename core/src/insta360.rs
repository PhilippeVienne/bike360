//! Lecture des fichiers Insta360 X5 (.insv / .lrv).
//!
//! Le trailer Insta360 (fin de fichier, terminé par `MAGIC`) contient un index
//! d'enregistrements. Ceux utilisés ici :
//!   0x0101  métadonnées (protobuf) : first_frame_timestamp (champ 24, µs horloge IMU)
//!           et gyro_timestamp (champ 28, ms) donnent le calage exact IMU ↔ vidéo
//!   0x0003  IMU à 1 kHz : u64 timestamp µs + 6 × u16 décalés (acc xyz, gyro xyz)
//! Les .lrv (proxys) portent le même trailer que les .insv. Référence : telemetry-parser
//! (Gyroflow), src/insta360/.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::NaiveDateTime;
use regex::Regex;
use serde::Serialize;

const MAGIC: &[u8] = b"8db42d694ccc418790edff439fe026bf";
const ACC_LSB_PER_G: f64 = 1024.0; // acc_range = 32 g (métadonnées)
/// Écart toléré entre la fin d'un fichier et le début du suivant (heure du nom : à la seconde).
pub const MAX_CHAIN_GAP_S: f64 = 2.5;

/// Enregistrements du trailer {identifiant: contenu} (0x03 et 0x0101), vide s'il est absent.
pub fn read_records(path: &Path) -> Result<HashMap<u16, Vec<u8>>> {
    let mut f = File::open(path).with_context(|| format!("ouverture de {path:?}"))?;
    let size = f.seek(SeekFrom::End(0))?;
    let mut out = HashMap::new();
    if size < 78 + 310 {
        return Ok(out);
    }
    let mut tail = [0u8; 78];
    f.seek(SeekFrom::Start(size - 78))?;
    f.read_exact(&mut tail)?;
    if &tail[78 - 32..] != MAGIC {
        return Ok(out);
    }
    let start = size - u32::from_le_bytes(tail[38..42].try_into()?) as u64;
    let mut index = [0u8; 310];
    f.seek(SeekFrom::Start(size - 78 - 310))?;
    f.read_exact(&mut index)?;
    for i in (10..310).step_by(10) {
        let rid = u16::from_le_bytes(index[i..i + 2].try_into()?);
        let length = u32::from_le_bytes(index[i + 2..i + 6].try_into()?) as usize;
        let offset = u32::from_le_bytes(index[i + 6..i + 10].try_into()?) as u64;
        if length > 0 && (rid == 0x03 || rid == 0x0101) {
            let mut buf = vec![0u8; length];
            f.seek(SeekFrom::Start(start + offset))?;
            f.read_exact(&mut buf)?;
            out.insert(rid, buf);
        }
    }
    Ok(out)
}

/// Valeur d'un champ protobuf de premier niveau.
#[derive(Debug, Clone)]
pub enum Field {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(Vec<u8>),
    Fixed32([u8; 4]),
}

/// Champs de premier niveau d'un message protobuf : {numéro: [valeurs]}.
pub fn protobuf_fields(b: &[u8]) -> HashMap<u64, Vec<Field>> {
    let mut i = 0usize;
    let mut out: HashMap<u64, Vec<Field>> = HashMap::new();
    let varint = |i: &mut usize| -> Option<u64> {
        let (mut v, mut s) = (0u64, 0u32);
        loop {
            let c = *b.get(*i)?;
            *i += 1;
            v |= ((c & 0x7F) as u64) << s;
            s += 7;
            if c < 0x80 {
                return Some(v);
            }
        }
    };
    while i < b.len() {
        let Some(key) = varint(&mut i) else { break };
        let (tag, wire) = (key >> 3, key & 7);
        let val = match wire {
            0 => match varint(&mut i) {
                Some(v) => Field::Varint(v),
                None => break,
            },
            1 if i + 8 <= b.len() => {
                i += 8;
                Field::Fixed64(b[i - 8..i].try_into().unwrap())
            }
            2 => {
                let Some(n) = varint(&mut i) else { break };
                let n = n as usize;
                if i + n > b.len() {
                    break;
                }
                i += n;
                Field::Bytes(b[i - n..i].to_vec())
            }
            5 if i + 4 <= b.len() => {
                i += 4;
                Field::Fixed32(b[i - 4..i].try_into().unwrap())
            }
            _ => break,
        };
        out.entry(tag).or_default().push(val);
    }
    out
}

/// Mesures IMU d'un fichier : temps vidéo (s), accélération (g), gyroscope brut centré.
pub struct Imu {
    pub t: Vec<f64>,
    pub acc: Vec<[f64; 3]>,
    pub gyro: Vec<[f64; 3]>,
}

/// IMU calée sur la vidéo : t = (ts − first_frame_timestamp)/1e6 − gyro_timestamp/1000.
pub fn read_imu(path: &Path) -> Result<Imu> {
    let rec = read_records(path)?;
    let meta = protobuf_fields(rec.get(&0x0101).context("métadonnées Insta360 absentes")?);
    let first_frame = match meta.get(&24).and_then(|v| v.first()) {
        Some(Field::Varint(v)) => *v as i64,
        _ => bail!("first_frame_timestamp absent"),
    };
    let gyro_ts = match meta.get(&28).and_then(|v| v.first()) {
        Some(Field::Fixed64(b)) => f64::from_le_bytes(*b),
        _ => 0.0,
    };
    let raw = rec.get(&0x03).context("IMU absente")?;
    let n = raw.len() / 20;
    let (mut t, mut acc, mut gyro) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
    for r in raw.chunks_exact(20) {
        let ts = u64::from_le_bytes(r[0..8].try_into()?) as i64;
        let v: Vec<f64> = (0..6).map(|k| u16::from_le_bytes([r[8 + 2 * k], r[9 + 2 * k]]) as f64 - 32768.0).collect();
        t.push((ts - first_frame) as f64 / 1e6 - gyro_ts / 1000.0);
        acc.push([v[0] / ACC_LSB_PER_G, v[1] / ACC_LSB_PER_G, v[2] / ACC_LSB_PER_G]);
        gyro.push([v[3], v[4], v[5]]);
    }
    Ok(Imu { t, acc, gyro })
}

#[derive(Debug, Clone, Serialize)]
pub struct Segment {
    pub index: u32,
    pub insv: Option<PathBuf>,
    pub lrv: Option<PathBuf>,
    /// Début du segment dans le temps de session (s), durée (s).
    pub offset: f64,
    pub duration: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    /// ex. VID_20260829_112347
    pub id: String,
    /// YYYYMMDD et HHMMSS (heure locale caméra)
    pub date: String,
    pub time: String,
    pub segments: Vec<Segment>,
    /// Sessions d'origine fusionnées (enregistrement en boucle).
    pub parts: Vec<String>,
}

impl Session {
    pub fn start(&self) -> Option<NaiveDateTime> {
        NaiveDateTime::parse_from_str(&format!("{}{}", self.date, self.time), "%Y%m%d%H%M%S").ok()
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Regroupe les fichiers d'un dossier (et sous-dossiers) en sessions (même horodatage de départ).
pub fn scan(dcim: &Path) -> Vec<Session> {
    let re = Regex::new(r"(?i)^(VID|LRV)_(\d{8})_(\d{6})_(\d{2})_(\d{3})\.(insv|lrv)$").unwrap();
    let mut files = Vec::new();
    walk(dcim, &mut files);
    files.sort();
    let mut sessions: Vec<Session> = Vec::new();
    for p in files {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(m) = re.captures(name) else { continue };
        let (kind, date, time, idx) = (&m[1], &m[2], &m[3], m[5].parse::<u32>().unwrap_or(0));
        let sid = format!("VID_{date}_{time}");
        let s = match sessions.iter_mut().position(|s| s.id == sid) {
            Some(i) => &mut sessions[i],
            None => {
                sessions.push(Session { id: sid, date: date.into(), time: time.into(), segments: vec![], parts: vec![] });
                sessions.last_mut().unwrap()
            }
        };
        let seg = match s.segments.iter_mut().position(|x| x.index == idx) {
            Some(i) => &mut s.segments[i],
            None => {
                s.segments.push(Segment { index: idx, insv: None, lrv: None, offset: 0.0, duration: 0.0 });
                s.segments.last_mut().unwrap()
            }
        };
        if kind.eq_ignore_ascii_case("VID") {
            seg.insv = Some(p.clone());
        } else {
            seg.lrv = Some(p.clone());
        }
    }
    for s in &mut sessions {
        s.segments.sort_by_key(|x| x.index);
    }
    sessions.retain(|s| s.segments.iter().all(|seg| seg.lrv.is_some()));
    sessions
}

/// Fusionne les sessions qui se suivent sans interruption (enregistrement en boucle).
///
/// En mode boucle, la caméra écrit des fichiers d'une minute effacés au fil de l'eau : chacun
/// forme une « session » dont l'heure de début suit exactement la fin de la précédente. On les
/// réunit en un bloc continu, identifié par sa première session ; `parts` garde la liste
/// d'origine et les segments sont renumérotés dans l'ordre.
pub fn merge_continuous(mut sessions: Vec<Session>, duration_of: impl Fn(&Session) -> f64) -> Vec<Session> {
    sessions.sort_by(|a, b| (a.date.clone() + &a.time).cmp(&(b.date.clone() + &b.time)));
    let mut out: Vec<(Session, NaiveDateTime, f64)> = Vec::new();
    for s in sessions {
        let Some(start) = s.start() else { continue };
        let dur = duration_of(&s);
        if let Some((block, prev_start, prev_dur)) = out.last_mut() {
            let gap = (start - *prev_start).num_milliseconds() as f64 / 1000.0 - *prev_dur;
            if gap.abs() <= MAX_CHAIN_GAP_S {
                if block.parts.is_empty() {
                    block.parts.push(block.id.clone());
                }
                block.parts.push(s.id.clone());
                block.segments.extend(s.segments);
                *prev_start = start;
                *prev_dur = dur;
                continue;
            }
        }
        out.push((s, start, dur));
    }
    let mut blocks: Vec<Session> = out.into_iter().map(|(b, _, _)| b).collect();
    for b in &mut blocks {
        if !b.parts.is_empty() {
            for (i, seg) in b.segments.iter_mut().enumerate() {
                seg.index = i as u32 + 1;
            }
        }
    }
    blocks
}
