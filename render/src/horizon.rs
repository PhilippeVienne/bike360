//! Analyse d'horizon sur GPU (portage de horizon.py, étape « émissions » du Viterbi).
//!
//! Usage : `insta-render horizon job.json`. Décode un .lrv (NVDEC), et pour chaque instant
//! start + j/hz : image équirect → contours → score de chaque orientation candidate.
//! Sortie : flottants f32 little-endian, `states.len()` par instant (scores bruts, non
//! normalisés), une ligne `frame=j` par instant sur la sortie standard.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use cudarc::driver::sys as cu;
use cudarc::driver::{CudaContext, DeviceRepr, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;
use nvidia_video_codec_sdk::sys::cuviddec::cudaVideoCodec;
use serde::Deserialize;

use crate::{mp4, nvdec};

const LENS_FOV_DEG: f32 = 195.0;

#[derive(Deserialize)]
struct Job {
    source: PathBuf,
    start: f64,
    duration: f64,
    hz: f64,
    /// Repère image équirect → repère caméra (inclinaison fixe), ligne par ligne.
    base: [f32; 9],
    /// Orientation géométrique de chaque état candidat (cf. horizon.GEOMETRIC).
    states: Vec<[f32; 3]>,
    sigma: f32,
    width: u32,
    height: u32,
    lat_min: f32,
    lat_max: f32,
    excl_lon: [f32; 2],
    grad_min: f32,
    weight_cap: f32,
    output: PathBuf,
}

/// Doit correspondre à `struct EqParams` de horizon.cu.
#[repr(C)]
#[derive(Clone, Copy)]
struct EqParams {
    y: u64,
    in_w: i32,
    in_h: i32,
    pitch: i32,
    out: u64,
    w: i32,
    h: i32,
    m: [f32; 9],
    lens_half: f32,
}
unsafe impl DeviceRepr for EqParams {}

/// Doit correspondre à `struct EdgeParams` de horizon.cu.
#[repr(C)]
#[derive(Clone, Copy)]
struct EdgeParams {
    img: u64,
    w: i32,
    h: i32,
    out: u64,
    lat_min: f32,
    lat_max: f32,
    excl_lon0: f32,
    excl_lon1: f32,
    grad_min: f32,
    weight_cap: f32,
}
unsafe impl DeviceRepr for EdgeParams {}

fn alloc(bytes: usize) -> Result<u64> {
    let mut p = 0u64;
    let r = unsafe { cu::cuMemAlloc_v2(&mut p, bytes) };
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("cuMemAlloc : {r:?}");
    }
    Ok(p)
}

pub fn run(path: &str) -> Result<()> {
    let job: Job = serde_json::from_reader(File::open(path)?).context("lecture du job horizon")?;
    let ctx = CudaContext::new(0)?;
    ctx.bind_to_thread()?;
    let stream = ctx.default_stream();
    let module = ctx.load_module(Ptx::from_src(include_str!(concat!(env!("OUT_DIR"), "/horizon.ptx"))))?;
    let (k_eq, k_edges, k_scores) =
        (module.load_function("equirect")?, module.load_function("edges")?, module.load_function("scores")?);

    let mut m = mp4::Mp4::open(&job.source)?;
    let ti = *m.video_tracks().first().context("aucune piste vidéo")?;
    let codec = if &m.tracks[ti].codec == b"avc1" { cudaVideoCodec::cudaVideoCodec_H264 } else { cudaVideoCodec::cudaVideoCodec_HEVC };
    let track = &m.tracks[ti];
    let (ts, fd) = (track.timescale as f64, track.frame_duration());
    let first = track.sample_at(job.start);
    let last = track.sample_at(job.start + job.duration).min(track.samples.len());
    let dec_start = track.keyframe_before(first);
    let (nls, params_sets) = (track.nal_length_size, track.parameter_sets.clone());
    let samples: Vec<mp4::Sample> = track.samples[dec_start..last].to_vec();

    let (w, h) = (job.width as usize, job.height as usize);
    let (w2, h2) = (w / 2, h / 2);
    let n_states = job.states.len();
    let eq_buf = alloc(w * h * 4)?;
    let edge_buf = alloc(w2 * h2 * 16)?;
    let states_buf = alloc(n_states * 12)?;
    let scores_buf = alloc(n_states * 4)?;
    let flat: Vec<f32> = job.states.iter().flatten().copied().collect();
    unsafe {
        let r = cu::cuMemcpyHtoD_v2(states_buf, flat.as_ptr() as *const _, flat.len() * 4);
        if r != cu::CUresult::CUDA_SUCCESS {
            bail!("copie des états : {r:?}");
        }
    }
    let mut host = vec![0f32; n_states];
    let mut out = BufWriter::new(File::create(&job.output)?);
    let stdout = std::io::stdout();

    let cfg2d = |cw: usize, ch: usize| LaunchConfig {
        grid_dim: ((cw as u32).div_ceil(16), (ch as u32).div_ceil(16), 1),
        block_dim: (16, 16, 1),
        shared_mem_bytes: 0,
    };
    let n_out = (job.duration * job.hz).floor() as usize;
    let mut j = 0usize;
    let mut process = |f: &nvdec::Frame, j: usize, out: &mut BufWriter<File>| -> Result<()> {
        let p = EqParams {
            y: f.dptr, in_w: f.pitch as i32, in_h: f.height as i32, pitch: f.pitch as i32, out: eq_buf,
            w: w as i32, h: h as i32, m: job.base, lens_half: (LENS_FOV_DEG / 2.0).to_radians(),
        };
        let mut b = stream.launch_builder(&k_eq);
        b.arg(&p);
        unsafe { b.launch(cfg2d(w, h))? };
        let e = EdgeParams {
            img: eq_buf, w: w as i32, h: h as i32, out: edge_buf, lat_min: job.lat_min, lat_max: job.lat_max,
            excl_lon0: job.excl_lon[0], excl_lon1: job.excl_lon[1], grad_min: job.grad_min, weight_cap: job.weight_cap,
        };
        let mut b = stream.launch_builder(&k_edges);
        b.arg(&e);
        unsafe { b.launch(cfg2d(w2, h2))? };
        let n_edges = (w2 * h2) as i32;
        let inv_s2 = 1.0f32 / (job.sigma * job.sigma);
        let mut b = stream.launch_builder(&k_scores);
        b.arg(&edge_buf).arg(&n_edges).arg(&states_buf).arg(&inv_s2).arg(&scores_buf);
        unsafe {
            b.launch(LaunchConfig { grid_dim: (n_states as u32, 1, 1), block_dim: (256, 1, 1), shared_mem_bytes: 0 })?
        };
        stream.synchronize()?;
        unsafe {
            let r = cu::cuMemcpyDtoH_v2(host.as_mut_ptr() as *mut _, scores_buf, n_states * 4);
            if r != cu::CUresult::CUDA_SUCCESS {
                bail!("copie des scores : {r:?}");
            }
        }
        for v in &host {
            out.write_all(&v.to_le_bytes())?;
        }
        writeln!(stdout.lock(), "frame={}", j + 1)?;
        Ok(())
    };

    let mut dec = nvdec::Decoder::new(ctx.cu_ctx(), codec)?;
    let (mut raw, mut au) = (Vec::new(), Vec::new());
    let sample_time = |idx: i64| m_time(&samples, dec_start, idx, ts);
    // Attribue chaque image décodée aux instants d'analyse qui tombent dans sa durée.
    let mut drain = |dec: &mut nvdec::Decoder, j: &mut usize, out: &mut BufWriter<File>| -> Result<()> {
        while let Some(f) = dec.pop() {
            let t = sample_time(f.pts);
            while *j < n_out && job.start + *j as f64 / job.hz < t + fd / 2.0 {
                if job.start + *j as f64 / job.hz >= t - fd / 2.0 {
                    process(&f, *j, out)?;
                } else {
                    for _ in 0..n_states {
                        out.write_all(&0f32.to_le_bytes())?; // instant sans image
                    }
                }
                *j += 1;
            }
            dec.recycle(f);
        }
        Ok(())
    };
    for (k, s) in samples.iter().enumerate() {
        if j >= n_out {
            break;
        }
        m.read_sample(s, &mut raw)?;
        au.clear();
        if k == 0 {
            for ps in &params_sets {
                au.extend_from_slice(&[0, 0, 0, 1]);
                au.extend_from_slice(ps);
            }
        }
        mp4::to_annexb(&raw, nls, &mut au);
        dec.feed(&au, (dec_start + k) as i64)?;
        drain(&mut dec, &mut j, &mut out)?;
    }
    dec.feed(&[], 0)?;
    drain(&mut dec, &mut j, &mut out)?;
    drop(drain);
    out.flush()?;
    unsafe {
        for b in [eq_buf, edge_buf, states_buf, scores_buf] {
            let _ = cu::cuMemFree_v2(b);
        }
    }
    eprintln!("{j} instants analysés ({n_out} demandés)");
    Ok(())
}

fn m_time(samples: &[mp4::Sample], base: usize, idx: i64, timescale: f64) -> f64 {
    let i = (idx as usize).saturating_sub(base).min(samples.len().saturating_sub(1));
    samples[i].dts as f64 / timescale
}
