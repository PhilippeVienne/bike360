//! Décodage HEVC / H.264 matériel (NVDEC) vers des images NV12 en mémoire GPU.
//!
//! Le parseur NVIDIA appelle trois fonctions de rappel : séquence (création du
//! décodeur), décodage d'une image, affichage d'une image prête. À l'affichage, on
//! copie la surface du décodeur dans un tampon à nous (copie GPU→GPU, négligeable),
//! ce qui libère la surface et découple le décodage du rendu.

// Les fonctions de rappel sont entièrement des appels FFI : blocs `unsafe` implicites.
#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::ptr;

use anyhow::{bail, Result};
use cudarc::driver::sys::{self as cu, CUdeviceptr};
use nvidia_video_codec_sdk::sys::cuviddec::*;
use nvidia_video_codec_sdk::sys::nvcuvid::*;

/// Image NV12 décodée : luminance (width × height, pas `pitch`) puis chrominance entrelacée.
pub struct Frame {
    pub dptr: CUdeviceptr,
    pub pitch: usize,
    pub height: usize,
    /// Horodatage transmis au parseur (index d'échantillon dans la piste).
    pub pts: i64,
}

struct State {
    decoder: CUvideodecoder,
    lock: CUvideoctxlock,
    width: usize,
    height: usize,
    pool: Vec<CUdeviceptr>,
    ready: VecDeque<Frame>,
    error: Option<String>,
}

pub struct Decoder {
    parser: CUvideoparser,
    state: Box<State>,
}

fn check(r: cu::CUresult, what: &str) -> Result<()> {
    if r != cu::CUresult::CUDA_SUCCESS {
        bail!("{what} : {r:?}");
    }
    Ok(())
}

/// Copie 2D GPU→GPU (la structure contient des énumérations : pas de `zeroed`).
pub fn copy_2d(src: CUdeviceptr, src_pitch: usize, dst: CUdeviceptr, dst_pitch: usize, width: usize, rows: usize) -> cu::CUDA_MEMCPY2D {
    cu::CUDA_MEMCPY2D {
        srcXInBytes: 0,
        srcY: 0,
        srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
        srcHost: ptr::null(),
        srcDevice: src,
        srcArray: ptr::null_mut(),
        srcPitch: src_pitch,
        dstXInBytes: 0,
        dstY: 0,
        dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
        dstHost: ptr::null_mut(),
        dstDevice: dst,
        dstArray: ptr::null_mut(),
        dstPitch: dst_pitch,
        WidthInBytes: width,
        Height: rows,
    }
}

unsafe extern "C" fn on_sequence(user: *mut c_void, fmt: *mut CUVIDEOFORMAT) -> c_int {
    let st = &mut *(user as *mut State);
    let f = &*fmt;
    let surfaces = f.min_num_decode_surfaces as u64 + 4;
    if !st.decoder.is_null() {
        return surfaces as c_int;
    }
    let (w, h) = ((f.display_area.right - f.display_area.left) as u64, (f.display_area.bottom - f.display_area.top) as u64);
    let mut info: CUVIDDECODECREATEINFO = std::mem::zeroed();
    info.ulWidth = f.coded_width as _;
    info.ulHeight = f.coded_height as _;
    info.ulNumDecodeSurfaces = surfaces as _;
    info.CodecType = f.codec;
    info.ChromaFormat = f.chroma_format;
    info.ulCreationFlags = cudaVideoCreateFlags::cudaVideoCreate_PreferCUVID as _;
    info.bitDepthMinus8 = f.bit_depth_luma_minus8 as _;
    info.ulMaxWidth = f.coded_width as _;
    info.ulMaxHeight = f.coded_height as _;
    info.display_area.left = f.display_area.left as _;
    info.display_area.top = f.display_area.top as _;
    info.display_area.right = f.display_area.right as _;
    info.display_area.bottom = f.display_area.bottom as _;
    info.OutputFormat = cudaVideoSurfaceFormat::cudaVideoSurfaceFormat_NV12;
    info.DeinterlaceMode = cudaVideoDeinterlaceMode::cudaVideoDeinterlaceMode_Weave;
    info.ulTargetWidth = w as _;
    info.ulTargetHeight = h as _;
    info.ulNumOutputSurfaces = 2;
    info.vidLock = st.lock;
    let r = cuvidCreateDecoder(&mut st.decoder, &mut info);
    if r != cu::CUresult::CUDA_SUCCESS {
        st.error = Some(format!("cuvidCreateDecoder : {r:?}"));
        return 0;
    }
    st.width = w as usize;
    st.height = h as usize;
    surfaces as c_int
}

unsafe extern "C" fn on_decode(user: *mut c_void, pic: *mut CUVIDPICPARAMS) -> c_int {
    let st = &mut *(user as *mut State);
    let r = cuvidDecodePicture(st.decoder, pic);
    if r != cu::CUresult::CUDA_SUCCESS {
        st.error = Some(format!("cuvidDecodePicture : {r:?}"));
        return 0;
    }
    1
}

unsafe extern "C" fn on_display(user: *mut c_void, disp: *mut CUVIDPARSERDISPINFO) -> c_int {
    let st = &mut *(user as *mut State);
    if disp.is_null() {
        return 1; // fin de flux
    }
    let d = &*disp;
    let mut proc_params: CUVIDPROCPARAMS = std::mem::zeroed();
    proc_params.progressive_frame = d.progressive_frame;
    proc_params.top_field_first = d.top_field_first;
    let (mut src, mut pitch) = (0u64, 0u32);
    let r = cuvidMapVideoFrame64(st.decoder, d.picture_index, &mut src, &mut pitch, &mut proc_params);
    if r != cu::CUresult::CUDA_SUCCESS {
        st.error = Some(format!("cuvidMapVideoFrame64 : {r:?}"));
        return 0;
    }
    let (w, h) = (st.width, st.height);
    let dst = match st.pool.pop() {
        Some(p) => p,
        None => {
            let mut p = 0;
            if cu::cuMemAlloc_v2(&mut p, w * h * 3 / 2) != cu::CUresult::CUDA_SUCCESS {
                st.error = Some("cuMemAlloc".into());
                let _ = cuvidUnmapVideoFrame64(st.decoder, src);
                return 0;
            }
            p
        }
    };
    // Luminance puis chrominance (la chrominance suit la hauteur de surface, arrondie au pair).
    for (so, doff, rows) in [(0usize, 0usize, h), (pitch as usize * ((h + 1) & !1), w * h, h / 2)] {
        let c = copy_2d(src + so as u64, pitch as usize, dst + doff as u64, w, w, rows);
        if cu::cuMemcpy2D_v2(&c) != cu::CUresult::CUDA_SUCCESS {
            st.error = Some("cuMemcpy2D".into());
        }
    }
    let _ = cuvidUnmapVideoFrame64(st.decoder, src);
    st.ready.push_back(Frame { dptr: dst, pitch: w, height: h, pts: d.timestamp });
    1
}

impl Decoder {
    /// Le contexte CUDA doit être actif sur le thread appelant.
    pub fn new(ctx: cu::CUcontext, codec: cudaVideoCodec) -> Result<Self> {
        let mut state = Box::new(State {
            decoder: ptr::null_mut(),
            lock: ptr::null_mut(),
            width: 0,
            height: 0,
            pool: Vec::new(),
            ready: VecDeque::new(),
            error: None,
        });
        unsafe {
            check(cuvidCtxLockCreate(&mut state.lock, ctx), "cuvidCtxLockCreate")?;
            let mut p: CUVIDPARSERPARAMS = std::mem::zeroed();
            p.CodecType = codec;
            p.ulMaxNumDecodeSurfaces = 1;
            p.ulMaxDisplayDelay = 0;
            p.pUserData = &mut *state as *mut State as *mut c_void;
            p.pfnSequenceCallback = Some(on_sequence);
            p.pfnDecodePicture = Some(on_decode);
            p.pfnDisplayPicture = Some(on_display);
            let mut parser = ptr::null_mut();
            check(cuvidCreateVideoParser(&mut parser, &mut p), "cuvidCreateVideoParser")?;
            Ok(Self { parser, state })
        }
    }

    /// Envoie une unité d'accès Annex-B (ou la fin de flux si `data` est vide).
    pub fn feed(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let mut pkt: CUVIDSOURCEDATAPACKET = unsafe { std::mem::zeroed() };
        if data.is_empty() {
            pkt.flags = CUvideopacketflags::CUVID_PKT_ENDOFSTREAM as _;
        } else {
            pkt.flags = CUvideopacketflags::CUVID_PKT_TIMESTAMP as _;
            pkt.payload_size = data.len() as _;
            pkt.payload = data.as_ptr();
            pkt.timestamp = pts;
        }
        unsafe { check(cuvidParseVideoData(self.parser, &mut pkt), "cuvidParseVideoData")? };
        if let Some(e) = self.state.error.take() {
            bail!(e);
        }
        Ok(())
    }

    pub fn pop(&mut self) -> Option<Frame> {
        self.state.ready.pop_front()
    }

    /// Rend un tampon d'image au pool pour réutilisation.
    pub fn recycle(&mut self, f: Frame) {
        self.state.pool.push(f.dptr);
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = cuvidDestroyVideoParser(self.parser);
            if !self.state.decoder.is_null() {
                let _ = cuvidDestroyDecoder(self.state.decoder);
            }
            for f in self.state.ready.drain(..) {
                let _ = cu::cuMemFree_v2(f.dptr);
            }
            for p in self.state.pool.drain(..) {
                let _ = cu::cuMemFree_v2(p);
            }
            let _ = cuvidCtxLockDestroy(self.state.lock);
        }
    }
}
