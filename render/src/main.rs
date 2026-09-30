//! Moteur de rendu GPU des clips Insta360 X5 : NVDEC → reprojection CUDA → NVENC.
//!
//! Usage : `bike360-render job.json` (rendu) ou `bike360-render horizon job.json` (analyse d'horizon, cf. horizon.rs). Le travail décrit un morceau de fichier .insv à
//! rendre en vue plane ; les rotations image par image (vue, horizon, inclinaison de
//! la caméra) sont calculées côté serveur et fournies telles quelles.
//! Sortie : un flux H.264 Annex-B (`output`), à multiplexer avec le son par ffmpeg.
//! Progression : une ligne `frame=N` par image sur la sortie standard.

mod horizon;
mod mp4;
mod nvdec;
mod postfx;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use cudarc::driver::sys as cu;
use cudarc::driver::{CudaContext, CudaFunction, CudaStream, DeviceRepr, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;
use nvidia_video_codec_sdk::sys::nvEncodeAPI::*;
use nvidia_video_codec_sdk::{EncodePictureParams, Encoder, EncoderInitParams};
use serde::Deserialize;

const LENS_FOV_DEG: f32 = 195.0;

#[derive(Deserialize)]
struct Job {
    source: PathBuf,
    /// Début du morceau dans le fichier (s) et durée (s).
    start: f64,
    duration: f64,
    width: u32,
    height: u32,
    /// Champ horizontal (degrés).
    fov: f32,
    /// Champ horizontal par image (points clés) ; à défaut, `fov` pour toutes.
    #[serde(default)]
    fovs: Vec<f32>,
    /// Mode « sélection » (hyperlapse) : indices d'échantillons à rendre, croissants ; les
    /// matrices/champs correspondent à cette liste. Vide = plage continue start..start+duration.
    #[serde(default)]
    samples: Vec<usize>,
    /// Qualité NVENC (≈ CRF x264, plus bas = meilleur).
    cq: u8,
    /// Plafond de débit (bit/s), pour les contenus très mobiles (hyperlapse) ; 0 = qualité seule.
    #[serde(default)]
    max_bitrate: u32,
    /// Zones floutées (x, y, w, h) dans l'image côte à côte du .lrv (0..1).
    #[serde(default)]
    masks: Vec<[f32; 4]>,
    /// Rotation écran → caméra (ligne par ligne) pour chaque image de sortie.
    matrices: Vec<[f32; 9]>,
    /// Par image de sortie : zones à flouter (x, y, w, h) en pixels de sortie (confidentialité).
    #[serde(default)]
    blur: Vec<Vec<[f32; 4]>>,
    /// Images RGBA (PNG) à incruster (télémétrie) …
    #[serde(default)]
    sprites: Vec<String>,
    /// … et, par image de sortie, leurs placements (indice d'image, x, y, opacité).
    #[serde(default)]
    overlays: Vec<Vec<[f32; 4]>>,
    output: PathBuf,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct LensPtr {
    y: u64,
    uv: u64,
}

/// Doit correspondre exactement à `struct Params` de reproject.cu.
#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    front: LensPtr,
    back: LensPtr,
    in_w: i32,
    in_h: i32,
    out_y: u64,
    out_uv: u64,
    out_w: i32,
    out_h: i32,
    out_pitch: i32,
    m: [f32; 9],
    tan_h: f32,
    tan_v: f32,
    lens_half: f32,
    masks: u64,
    n_masks: i32,
}

unsafe impl DeviceRepr for Params {}

fn check(r: cu::CUresult, what: &str) -> Result<()> {
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("{what} : {r:?}");
    }
    Ok(())
}

fn lens(f: &nvdec::Frame) -> LensPtr {
    LensPtr { y: f.dptr, uv: f.dptr + (f.pitch * f.height) as u64 }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("horizon") => horizon::run(args.get(2).context("usage : bike360-render horizon job.json")?),
        Some(path) => render(path),
        None => bail!("usage : bike360-render job.json | bike360-render horizon job.json"),
    }
}

/// Rendu d'un morceau de .insv (voir l'en-tête du fichier).
fn render(path: &str) -> Result<()> {
    let mut job: Job = serde_json::from_reader(File::open(&path)?).context("lecture du job")?;
    if job.width % 2 != 0 || job.height % 2 != 0 || job.matrices.is_empty() {
        bail!("dimensions paires et au moins une matrice requises");
    }

    let ctx = CudaContext::new(0)?;
    ctx.bind_to_thread()?;
    let stream: Arc<CudaStream> = ctx.default_stream();
    let module = ctx.load_module(Ptx::from_src(include_str!(concat!(env!("OUT_DIR"), "/reproject.ptx"))))?;
    let kernel: CudaFunction = module.load_function("reproject")?;

    // --- Entrée : deux pistes vidéo (0 = objectif avant, 1 = arrière)
    let mut m = mp4::Mp4::open(&job.source)?;
    let vids = m.video_tracks();
    if vids.len() < 2 {
        bail!("fichier .insv attendu (deux pistes vidéo)");
    }
    let (ta, tb) = (vids[0], vids[1]);
    let fd = m.tracks[ta].frame_duration();
    let n_samples = m.tracks[ta].samples.len().min(m.tracks[tb].samples.len());
    let (first_out, last_out) = if job.samples.is_empty() {
        (m.tracks[ta].sample_at(job.start), m.tracks[ta].sample_at(job.start + job.duration).min(n_samples))
    } else {
        (job.samples[0], (job.samples[job.samples.len() - 1] + 1).min(n_samples))
    };
    // index de sortie de chaque échantillon voulu (mode sélection)
    let wanted: HashMap<usize, usize> = job.samples.iter().enumerate().map(|(i, &s)| (s, i)).collect();
    let expected = if job.samples.is_empty() { last_out - first_out } else { job.samples.len() };
    let first_dec = m.tracks[ta].keyframe_before(first_out).min(m.tracks[tb].keyframe_before(first_out));
    let (in_w, in_h) = (m.tracks[ta].width as usize, m.tracks[ta].height as usize);

    // --- Sortie : tampon NV12 enregistré auprès de NVENC
    let (ow, oh) = (job.width as usize, job.height as usize);
    let out_pitch = ow;
    let mut out_buf = 0u64;
    unsafe { check(cu::cuMemAlloc_v2(&mut out_buf, out_pitch * oh * 3 / 2), "cuMemAlloc sortie")? };
    let mut masks_buf = 0u64;
    if !job.masks.is_empty() {
        let flat: Vec<f32> = job.masks.iter().flatten().copied().collect();
        unsafe {
            check(cu::cuMemAlloc_v2(&mut masks_buf, flat.len() * 4), "cuMemAlloc masques")?;
            check(cu::cuMemcpyHtoD_v2(masks_buf, flat.as_ptr() as *const _, flat.len() * 4), "copie masques")?;
        }
    }

    let encoder = Encoder::initialize_with_cuda(ctx.clone()).map_err(|e| anyhow::anyhow!("NVENC : {e:?}"))?;
    let mut preset = encoder
        .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P5_GUID, NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY)
        .map_err(|e| anyhow::anyhow!("preset NVENC : {e:?}"))?;
    let cfg = &mut preset.presetCfg;
    let fps_num = (1.0 / fd * 1001.0).round() as u32; // 29.97 → 30000/1001, 24 → 24024/1001
    let gop = ((1.0 / fd) * 2.0).round() as u32;
    cfg.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
    cfg.gopLength = gop;
    cfg.frameIntervalP = 1; // pas d'images B : mode synchrone simple
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR;
    cfg.rcParams.targetQuality = job.cq;
    if job.max_bitrate > 0 {
        // qualité visée, mais débit plafonné (tampon VBV d'une seconde)
        cfg.rcParams.averageBitRate = job.max_bitrate / 3 * 2;
        cfg.rcParams.maxBitRate = job.max_bitrate;
        cfg.rcParams.vbvBufferSize = job.max_bitrate;
    } else {
        cfg.rcParams.averageBitRate = 0;
        cfg.rcParams.maxBitRate = 80_000_000;
    }
    unsafe {
        let h264 = &mut cfg.encodeCodecConfig.h264Config;
        h264.idrPeriod = gop;
        h264.set_repeatSPSPPS(1);
        let vui = &mut h264.h264VUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
    }
    let mut init = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, job.width, job.height);
    init.preset_guid(NV_ENC_PRESET_P5_GUID)
        .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_HIGH_QUALITY)
        .framerate(fps_num, 1001)
        .enable_picture_type_decision()
        .encode_config(cfg);
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12, init)
        .map_err(|e| anyhow::anyhow!("session NVENC : {e:?}"))?;
    let mut input = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            out_buf as *mut std::ffi::c_void,
            out_pitch as u32,
        )
        .map_err(|e| anyhow::anyhow!("enregistrement tampon NVENC : {e:?}"))?;
    let mut bitstream = session.create_output_bitstream().map_err(|e| anyhow::anyhow!("bitstream : {e:?}"))?;
    let mut out = BufWriter::new(File::create(&job.output)?);

    // --- Boucle : décodage synchronisé des deux objectifs, rendu, encodage
    let tan_h = (job.fov.to_radians() / 2.0).tan();
    let tan_v = tan_h * oh as f32 / ow as f32;
    let mut base = Params {
        front: LensPtr { y: 0, uv: 0 },
        back: LensPtr { y: 0, uv: 0 },
        in_w: in_w as i32,
        in_h: in_h as i32,
        out_y: out_buf,
        out_uv: out_buf + (out_pitch * oh) as u64,
        out_w: ow as i32,
        out_h: oh as i32,
        out_pitch: out_pitch as i32,
        m: [0.0; 9],
        tan_h,
        tan_v,
        lens_half: (LENS_FOV_DEG / 2.0).to_radians(),
        masks: masks_buf,
        n_masks: job.masks.len() as i32,
    };
    let launch = LaunchConfig {
        grid_dim: ((ow as u32 / 2).div_ceil(16), (oh as u32 / 2).div_ceil(16), 1),
        block_dim: (16, 16, 1),
        shared_mem_bytes: 0,
    };
    // floutage et incrustations appliqués sur le GPU après la reprojection, avant l'encodage
    let fx = postfx::PostFx::new(&ctx, stream.clone(), &job.sprites, std::mem::take(&mut job.blur),
                                 std::mem::take(&mut job.overlays))?;
    let out_frame = postfx::Frame { y: out_buf, uv: out_buf + (out_pitch * oh) as u64, w: ow as i32,
                                    h: oh as i32, pitch: out_pitch as i32 };

    let hevc = nvidia_video_codec_sdk::sys::cuviddec::cudaVideoCodec::cudaVideoCodec_HEVC;
    let mut dec_a = nvdec::Decoder::new(ctx.cu_ctx(), hevc)?;
    let mut dec_b = nvdec::Decoder::new(ctx.cu_ctx(), hevc)?;
    let (mut ready_a, mut ready_b): (HashMap<i64, nvdec::Frame>, HashMap<i64, nvdec::Frame>) = (HashMap::new(), HashMap::new());
    let mut next = first_dec as i64;
    let (mut raw, mut au) = (Vec::new(), Vec::new());
    let mut encoded = 0usize;
    let mut skipped = 0usize;
    const STALL_FRAMES: usize = 12;
    let stdout = std::io::stdout();

    let mut render = |fa: &nvdec::Frame, fb: &nvdec::Frame, idx: usize, out: &mut BufWriter<File>| -> Result<()> {
        let mut p = base;
        p.front = lens(fa);
        p.back = lens(fb);
        p.m = job.matrices[idx.min(job.matrices.len() - 1)];
        if let Some(&f) = job.fovs.get(idx).or(job.fovs.last()) {
            p.tan_h = (f.to_radians() / 2.0).tan();
            p.tan_v = p.tan_h * oh as f32 / ow as f32;
        }
        let mut b = stream.launch_builder(&kernel);
        b.arg(&p);
        unsafe { b.launch(launch)? };
        if let Some(fx) = &fx {
            fx.apply(out_frame, idx)?;
        }
        stream.synchronize()?;
        session
            .encode_picture(&mut input, &mut bitstream, EncodePictureParams { input_timestamp: idx as u64, ..Default::default() })
            .map_err(|e| anyhow::anyhow!("encodage : {e:?}"))?;
        let lock = bitstream.lock().map_err(|e| anyhow::anyhow!("bitstream : {e:?}"))?;
        out.write_all(lock.data())?;
        base = p;
        Ok(())
    };

    let mut drain = |dec_a: &mut nvdec::Decoder,
                     dec_b: &mut nvdec::Decoder,
                     ready_a: &mut HashMap<i64, nvdec::Frame>,
                     ready_b: &mut HashMap<i64, nvdec::Frame>,
                     next: &mut i64,
                     out: &mut BufWriter<File>|
     -> Result<()> {
        // les images antérieures à `next` (livrées après un saut) sont rendues au pool
        while let Some(f) = dec_a.pop() {
            if f.pts < *next { dec_a.recycle(f) } else { ready_a.insert(f.pts, f); }
        }
        while let Some(f) = dec_b.pop() {
            if f.pts < *next { dec_b.recycle(f) } else { ready_b.insert(f.pts, f); }
        }
        loop {
            if !(ready_a.contains_key(next) && ready_b.contains_key(next)) {
                // Image jamais livrée par un décodeur (RASL écartée après un saut…) : sans
                // cela `next` resterait bloqué et les images s'accumuleraient sans fin.
                if ready_a.len().max(ready_b.len()) <= STALL_FRAMES {
                    break;
                }
                for (ready, dec) in [(&mut *ready_a, &mut *dec_a), (&mut *ready_b, &mut *dec_b)] {
                    if let Some(f) = ready.remove(next) {
                        dec.recycle(f);
                    }
                }
                skipped += 1;
                *next += 1;
                continue;
            }
            let (fa, fb) = (ready_a.remove(next).unwrap(), ready_b.remove(next).unwrap());
            let idx = if wanted.is_empty() {
                (*next >= first_out as i64 && (*next as usize) < last_out).then(|| *next as usize - first_out)
            } else {
                wanted.get(&(*next as usize)).copied()
            };
            if let Some(idx) = idx {
                render(&fa, &fb, idx, out)?;
                encoded += 1;
                writeln!(stdout.lock(), "frame={encoded}")?;
            }
            dec_a.recycle(fa);
            dec_b.recycle(fb);
            *next += 1;
        }
        Ok(())
    };

    // En mode sélection, on saute directement à l'image clé précédant la prochaine image
    // voulue quand elle est loin (hyperlapse très accéléré) : on ne décode que le nécessaire.
    const JUMP_MIN: usize = 90;
    let mut wi = 0usize;
    let mut k = first_dec;
    while k < last_out {
        if !job.samples.is_empty() {
            while wi < job.samples.len() && job.samples[wi] < k {
                wi += 1;
            }
            if let Some(&target) = job.samples.get(wi) {
                let kf = m.tracks[ta].keyframe_before(target).min(m.tracks[tb].keyframe_before(target));
                // on attend que la précédente image voulue soit sortie du décodeur (latence de
                // quelques images) : sinon le saut la jetterait avec les images inutiles
                let prev_done = wi == 0 || next > job.samples[wi - 1] as i64;
                if target > k + JUMP_MIN && kf > k && prev_done {
                    k = kf;
                    next = kf as i64;
                    // images décodées devenues inutiles : rendues au pool
                    for key in ready_a.keys().copied().filter(|p| *p < next).collect::<Vec<_>>() {
                        let f = ready_a.remove(&key).unwrap();
                        dec_a.recycle(f);
                    }
                    for key in ready_b.keys().copied().filter(|p| *p < next).collect::<Vec<_>>() {
                        let f = ready_b.remove(&key).unwrap();
                        dec_b.recycle(f);
                    }
                }
            }
        }
        for (ti, dec) in [(ta, &mut dec_a), (tb, &mut dec_b)] {
            let s = m.tracks[ti].samples[k];
            m.read_sample(&s, &mut raw)?;
            au.clear();
            if k == first_dec {
                for ps in &m.tracks[ti].parameter_sets {
                    au.extend_from_slice(&[0, 0, 0, 1]);
                    au.extend_from_slice(ps);
                }
            }
            mp4::to_annexb(&raw, m.tracks[ti].nal_length_size, &mut au);
            dec.feed(&au, k as i64)?;
        }
        drain(&mut dec_a, &mut dec_b, &mut ready_a, &mut ready_b, &mut next, &mut out)?;
        k += 1;
    }
    dec_a.feed(&[], 0)?;
    dec_b.feed(&[], 0)?;
    drain(&mut dec_a, &mut dec_b, &mut ready_a, &mut ready_b, &mut next, &mut out)?;
    drop(drain);
    drop(render);

    session.end_of_stream().map_err(|e| anyhow::anyhow!("fin NVENC : {e:?}"))?;
    out.flush()?;
    for f in ready_a.into_values().chain(ready_b.into_values()) {
        unsafe { let _ = cu::cuMemFree_v2(f.dptr); };
    }
    unsafe {
        let _ = cu::cuMemFree_v2(out_buf);
        if masks_buf != 0 {
            let _ = cu::cuMemFree_v2(masks_buf);
        }
    }
    eprintln!("{encoded} images encodées ({expected} attendues, {skipped} manquantes sautées)");
    Ok(())
}
