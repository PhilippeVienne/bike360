//! Démultiplexeur MP4 minimal pour les .insv / .lrv de l'Insta360 X5.
//!
//! Lit la table des échantillons (stts/stss/stsz/stsc/stco|co64) de chaque piste et,
//! les ensembles de paramètres (VPS/SPS/PPS de `hvcC` pour le HEVC, SPS/PPS de `avcC` pour le H.264).
//! Les fichiers dépassent 4 Go : tailles de boîtes 64 bits et `co64` sont gérés.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Instant de décodage en unités de `timescale`.
    pub dts: u64,
    pub sync: bool,
}

#[derive(Debug, Default)]
pub struct Track {
    pub id: u32,
    pub handler: [u8; 4],
    pub codec: [u8; 4],
    pub timescale: u32,
    pub width: u16,
    pub height: u16,
    /// Taille (octets) du préfixe de longueur des NAL (hvcC.lengthSizeMinusOne + 1).
    pub nal_length_size: usize,
    /// VPS, SPS, PPS… (sans code de début).
    pub parameter_sets: Vec<Vec<u8>>,
    pub samples: Vec<Sample>,
}

impl Track {
    pub fn is_video(&self) -> bool {
        &self.handler == b"vide"
    }

    /// Index du premier échantillon dont l'instant (s) est ≥ `t`.
    pub fn sample_at(&self, t: f64) -> usize {
        let ticks = (t * self.timescale as f64).round() as u64;
        self.samples.partition_point(|s| s.dts < ticks)
    }

    /// Image clé précédant (ou égale à) l'échantillon `i`.
    pub fn keyframe_before(&self, i: usize) -> usize {
        (0..=i.min(self.samples.len().saturating_sub(1))).rev().find(|&k| self.samples[k].sync).unwrap_or(0)
    }

    pub fn frame_duration(&self) -> f64 {
        if self.samples.len() < 2 {
            return 0.0;
        }
        let n = self.samples.len() as f64 - 1.0;
        (self.samples.last().unwrap().dts - self.samples[0].dts) as f64 / n / self.timescale as f64
    }
}

pub struct Mp4 {
    file: File,
    pub tracks: Vec<Track>,
}

struct Boxes<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Itère sur les boîtes filles contenues dans un tampon.
impl<'a> Iterator for Boxes<'a> {
    type Item = ([u8; 4], &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        let d = &self.data[self.pos..];
        if d.len() < 8 {
            return None;
        }
        let mut size = u32::from_be_bytes(d[0..4].try_into().unwrap()) as usize;
        let kind: [u8; 4] = d[4..8].try_into().unwrap();
        let mut header = 8;
        if size == 1 {
            size = u64::from_be_bytes(d[8..16].try_into().unwrap()) as usize;
            header = 16;
        } else if size == 0 {
            size = d.len();
        }
        if size < header || size > d.len() {
            return None;
        }
        self.pos += size;
        Some((kind, &d[header..size]))
    }
}

fn boxes(data: &[u8]) -> Boxes<'_> {
    Boxes { data, pos: 0 }
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| k == kind).map(|(_, b)| b)
}

fn be32(d: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(d[at..at + 4].try_into().unwrap())
}

fn be64(d: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(d[at..at + 8].try_into().unwrap())
}

impl Mp4 {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("ouverture de {}", path.display()))?;
        let len = file.metadata()?.len();
        // Parcours des boîtes de premier niveau jusqu'à `moov` (sans lire `mdat`).
        let mut pos = 0u64;
        let moov = loop {
            if pos + 8 > len {
                bail!("boîte moov introuvable");
            }
            file.seek(SeekFrom::Start(pos))?;
            let mut h = [0u8; 16];
            file.read_exact(&mut h[..8])?;
            let mut size = u32::from_be_bytes(h[0..4].try_into()?) as u64;
            let mut header = 8;
            if size == 1 {
                file.read_exact(&mut h[8..16])?;
                size = u64::from_be_bytes(h[8..16].try_into()?);
                header = 16;
            } else if size == 0 {
                size = len - pos;
            }
            if &h[4..8] == b"moov" {
                let mut buf = vec![0u8; (size - header) as usize];
                file.read_exact(&mut buf)?;
                break buf;
            }
            pos += size;
        };
        let tracks = boxes(&moov).filter(|(k, _)| k == b"trak").map(|(_, t)| parse_trak(t)).collect::<Result<_>>()?;
        Ok(Self { file, tracks })
    }

    pub fn video_tracks(&self) -> Vec<usize> {
        (0..self.tracks.len()).filter(|&i| self.tracks[i].is_video()).collect()
    }

    pub fn read_sample(&mut self, s: &Sample, buf: &mut Vec<u8>) -> Result<()> {
        buf.resize(s.size as usize, 0);
        self.file.seek(SeekFrom::Start(s.offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
}

fn parse_trak(trak: &[u8]) -> Result<Track> {
    let mut t = Track::default();
    if let Some(tkhd) = child(trak, b"tkhd") {
        t.id = if tkhd[0] == 1 { be32(tkhd, 20) } else { be32(tkhd, 12) };
    }
    let mdia = child(trak, b"mdia").context("mdia")?;
    let mdhd = child(mdia, b"mdhd").context("mdhd")?;
    t.timescale = if mdhd[0] == 1 { be32(mdhd, 20) } else { be32(mdhd, 12) };
    if let Some(hdlr) = child(mdia, b"hdlr") {
        t.handler = hdlr[8..12].try_into()?;
    }
    let stbl = child(child(mdia, b"minf").context("minf")?, b"stbl").context("stbl")?;

    // stsd : codec, dimensions, hvcC
    let stsd = child(stbl, b"stsd").context("stsd")?;
    if let Some((codec, entry)) = boxes(&stsd[8..]).next() {
        t.codec = codec;
        if t.is_video() && entry.len() > 78 {
            t.width = u16::from_be_bytes(entry[24..26].try_into()?);
            t.height = u16::from_be_bytes(entry[26..28].try_into()?);
            if let Some(hvcc) = child(&entry[78..], b"hvcC") {
                parse_hvcc(hvcc, &mut t)?;
            } else if let Some(avcc) = child(&entry[78..], b"avcC") {
                parse_avcc(avcc, &mut t)?;
            }
        }
    }

    // Tailles
    let stsz = child(stbl, b"stsz").context("stsz")?;
    let fixed = be32(stsz, 4);
    let count = be32(stsz, 8) as usize;
    let sizes: Vec<u32> = if fixed != 0 { vec![fixed; count] } else { (0..count).map(|i| be32(stsz, 12 + 4 * i)).collect() };

    // Offsets des chunks
    let chunks: Vec<u64> = if let Some(co64) = child(stbl, b"co64") {
        (0..be32(co64, 4) as usize).map(|i| be64(co64, 8 + 8 * i)).collect()
    } else {
        let stco = child(stbl, b"stco").context("stco")?;
        (0..be32(stco, 4) as usize).map(|i| be32(stco, 8 + 4 * i) as u64).collect()
    };

    // Échantillons par chunk
    let stsc = child(stbl, b"stsc").context("stsc")?;
    let runs: Vec<(u32, u32)> = (0..be32(stsc, 4) as usize).map(|i| (be32(stsc, 8 + 12 * i), be32(stsc, 12 + 12 * i))).collect();

    // Durées
    let stts = child(stbl, b"stts").context("stts")?;
    let mut deltas = Vec::with_capacity(count);
    for i in 0..be32(stts, 4) as usize {
        let (n, d) = (be32(stts, 8 + 8 * i), be32(stts, 12 + 8 * i));
        deltas.extend(std::iter::repeat_n(d, n as usize));
    }

    let mut sync = vec![child(stbl, b"stss").is_none(); count];
    if let Some(stss) = child(stbl, b"stss") {
        for i in 0..be32(stss, 4) as usize {
            let k = be32(stss, 8 + 4 * i) as usize;
            if k >= 1 && k <= count {
                sync[k - 1] = true;
            }
        }
    }

    t.samples.reserve(count);
    let mut idx = 0usize;
    let mut dts = 0u64;
    for (c, &chunk_off) in chunks.iter().enumerate() {
        let chunk_no = c as u32 + 1;
        let per_chunk = runs.iter().rev().find(|(first, _)| *first <= chunk_no).map(|r| r.1).unwrap_or(0);
        let mut off = chunk_off;
        for _ in 0..per_chunk {
            if idx >= count {
                break;
            }
            t.samples.push(Sample { offset: off, size: sizes[idx], dts, sync: sync[idx] });
            off += sizes[idx] as u64;
            dts += *deltas.get(idx).unwrap_or(&0) as u64;
            idx += 1;
        }
    }
    Ok(t)
}

fn parse_hvcc(d: &[u8], t: &mut Track) -> Result<()> {
    if d.len() < 23 {
        bail!("hvcC trop court");
    }
    t.nal_length_size = (d[21] & 3) as usize + 1;
    let mut p = 23;
    for _ in 0..d[22] {
        let n = u16::from_be_bytes(d[p + 1..p + 3].try_into()?) as usize;
        p += 3;
        for _ in 0..n {
            let len = u16::from_be_bytes(d[p..p + 2].try_into()?) as usize;
            t.parameter_sets.push(d[p + 2..p + 2 + len].to_vec());
            p += 2 + len;
        }
    }
    Ok(())
}

fn parse_avcc(d: &[u8], t: &mut Track) -> Result<()> {
    if d.len() < 7 {
        bail!("avcC trop court");
    }
    t.nal_length_size = (d[4] & 3) as usize + 1;
    let mut p = 6;
    let mut read_sets = |count: usize, p: &mut usize| -> Result<()> {
        for _ in 0..count {
            let len = u16::from_be_bytes(d[*p..*p + 2].try_into()?) as usize;
            t.parameter_sets.push(d[*p + 2..*p + 2 + len].to_vec());
            *p += 2 + len;
        }
        Ok(())
    };
    read_sets((d[5] & 0x1f) as usize, &mut p)?; // SPS
    let n_pps = d[p] as usize;
    p += 1;
    read_sets(n_pps, &mut p)?; // PPS
    Ok(())
}

/// Convertit un échantillon (NAL préfixés par leur longueur) en flux Annex-B.
pub fn to_annexb(sample: &[u8], nal_length_size: usize, out: &mut Vec<u8>) {
    let mut p = 0;
    while p + nal_length_size <= sample.len() {
        let mut len = 0usize;
        for &b in &sample[p..p + nal_length_size] {
            len = (len << 8) | b as usize;
        }
        p += nal_length_size;
        if p + len > sample.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[p..p + len]);
        p += len;
    }
}
