//! Effets après reprojection (postfx.cu) : floutage de zones et incrustation d'images RGBA.
//!
//! Le job fournit, par image de sortie, les zones à flouter (confidentialité) et les images à
//! incruster avec leur position et opacité (télémétrie). Tout se fait sur le GPU, dans la même
//! passe que le rendu : pas de décodage/réencodage supplémentaire.

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use cudarc::driver::sys as cu;
use cudarc::driver::{CudaContext, CudaFunction, CudaStream, DeviceRepr, LaunchConfig, PushKernelArg};
use cudarc::nvrtc::Ptx;

use crate::check;

/// Correspondent aux structures de postfx.cu.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Frame {
    pub y: u64,
    pub uv: u64,
    pub w: i32,
    pub h: i32,
    pub pitch: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Blur {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    cell: i32,
    gw: i32,
    gh: i32,
    cells: u64,
    ix0: f32,
    iy0: f32,
    ix1: f32,
    iy1: f32,
    feather: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Sprite {
    rgba: u64,
    w: i32,
    h: i32,
    x: i32,
    y: i32,
    alpha: f32,
}

unsafe impl DeviceRepr for Frame {}
unsafe impl DeviceRepr for Blur {}
unsafe impl DeviceRepr for Sprite {}

const MAX_CELLS: usize = 64 * 64;   // grille maximale d'une zone floutée
const MIN_CELL: i32 = 4;

pub struct PostFx {
    stream: Arc<CudaStream>,
    blur_cells: CudaFunction,
    blur_apply: CudaFunction,
    composite: CudaFunction,
    cells: u64,
    sprites: Vec<(u64, i32, i32)>,
    blur: Vec<Vec<[f32; 4]>>,
    overlays: Vec<Vec<[f32; 4]>>,
}

fn load_png(path: &Path) -> Result<(Vec<u8>, u32, u32)> {
    let mut dec = png::Decoder::new(File::open(path).with_context(|| format!("image {path:?}"))?);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width, info.height);
    let px = &buf[..info.buffer_size()];
    let rgba = match info.color_type {
        png::ColorType::Rgba => px.to_vec(),
        png::ColorType::Rgb => px.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => px.chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => anyhow::bail!("format d'image non pris en charge : {other:?}"),
    };
    Ok((rgba, w, h))
}

impl PostFx {
    /// `None` si le job ne demande ni floutage ni incrustation.
    pub fn new(ctx: &Arc<CudaContext>, stream: Arc<CudaStream>, sprites: &[String],
               blur: Vec<Vec<[f32; 4]>>, overlays: Vec<Vec<[f32; 4]>>) -> Result<Option<Self>> {
        if blur.iter().all(|b| b.is_empty()) && overlays.iter().all(|o| o.is_empty()) {
            return Ok(None);
        }
        let module = ctx.load_module(Ptx::from_src(include_str!(concat!(env!("OUT_DIR"), "/postfx.ptx"))))?;
        let mut cells = 0u64;
        unsafe { check(cu::cuMemAlloc_v2(&mut cells, MAX_CELLS * 3 * 4), "cuMemAlloc cellules")? };
        let mut loaded = Vec::new();
        for path in sprites {
            let (rgba, w, h) = load_png(Path::new(path))?;
            let mut d = 0u64;
            unsafe {
                check(cu::cuMemAlloc_v2(&mut d, rgba.len()), "cuMemAlloc image")?;
                check(cu::cuMemcpyHtoD_v2(d, rgba.as_ptr() as *const _, rgba.len()), "copie image")?;
            }
            loaded.push((d, w as i32, h as i32));
        }
        Ok(Some(Self {
            stream,
            blur_cells: module.load_function("blur_cells")?,
            blur_apply: module.load_function("blur_apply")?,
            composite: module.load_function("composite")?,
            cells,
            sprites: loaded,
            blur,
            overlays,
        }))
    }

    /// Applique les effets de l'image de sortie `idx` (avant encodage).
    pub fn apply(&self, f: Frame, idx: usize) -> Result<()> {
        for b in self.blur.get(idx).map(Vec::as_slice).unwrap_or(&[]) {
            // fondu autour de la zone (¼ du petit côté) : la zone reste entièrement floutée,
            // le flou s'estompe au-delà au lieu de s'arrêter net
            let feather = (0.25 * b[2].min(b[3])).max(4.0);
            // zone + fondu, bornée à l'image, coordonnées paires (chrominance en 2×2)
            let x0 = ((b[0] - feather).max(0.0) as i32) & !1;
            let y0 = ((b[1] - feather).max(0.0) as i32) & !1;
            let x1 = ((b[0] + b[2] + feather).min(f.w as f32) as i32 + 1).min(f.w) & !1;
            let y1 = ((b[1] + b[3] + feather).min(f.h as f32) as i32 + 1).min(f.h) & !1;
            let (w, h) = (x1 - x0, y1 - y0);
            if w < 4 || h < 4 {
                continue;
            }
            // cellule ≈ 1/5 du plus grand côté : flou fort (plaque, visage illisibles)
            let mut cell = ((w.max(h) / 5).max(MIN_CELL) + 1) & !1;
            while ((w + cell - 1) / cell) as usize * ((h + cell - 1) / cell) as usize > MAX_CELLS {
                cell += 2;
            }
            let p = Blur { x: x0, y: y0, w, h, cell, gw: (w + cell - 1) / cell, gh: (h + cell - 1) / cell, cells: self.cells,
                           ix0: b[0], iy0: b[1], ix1: b[0] + b[2], iy1: b[1] + b[3], feather };
            let cfg = |gx: i32, gy: i32| LaunchConfig {
                grid_dim: ((gx as u32).div_ceil(16), (gy as u32).div_ceil(16), 1),
                block_dim: (16, 16, 1),
                shared_mem_bytes: 0,
            };
            let mut a = self.stream.launch_builder(&self.blur_cells);
            a.arg(&f).arg(&p);
            unsafe { a.launch(cfg(p.gw, p.gh))? };
            let mut c = self.stream.launch_builder(&self.blur_apply);
            c.arg(&f).arg(&p);
            unsafe { c.launch(cfg((w + 1) / 2, (h + 1) / 2))? };
        }
        for o in self.overlays.get(idx).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(&(rgba, w, h)) = self.sprites.get(o[0] as usize) else { continue };
            if o[3] <= 0.0 {
                continue;
            }
            let s = Sprite { rgba, w, h, x: o[1].round() as i32, y: o[2].round() as i32, alpha: o[3].min(1.0) };
            let gx = (((w + 3) / 2) as u32).div_ceil(16).max(1);
            let gy = (((h + 3) / 2) as u32).div_ceil(16).max(1);
            let cfg = LaunchConfig { grid_dim: (gx, gy, 1), block_dim: (16, 16, 1), shared_mem_bytes: 0 };
            let mut k = self.stream.launch_builder(&self.composite);
            k.arg(&f).arg(&s);
            unsafe { k.launch(cfg)? };
        }
        Ok(())
    }
}

impl Drop for PostFx {
    fn drop(&mut self) {
        unsafe {
            let _ = cu::cuMemFree_v2(self.cells);
            for &(d, _, _) in &self.sprites {
                let _ = cu::cuMemFree_v2(d);
            }
        }
    }
}
