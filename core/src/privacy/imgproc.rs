//! Traitements d'image nécessaires à la détection, reproduits d'OpenCV (sans dépendance) :
//! redimensionnement bilinéaire en virgule fixe (bit à bit identique à `cv2.resize` sur 8 bits),
//! moyenne 2×2 (INTER_AREA), bordures, masque TSV, composantes connexes, corrélation de gabarit,
//! flou gaussien.

/// Image 8 bits entrelacée (BGR pour 3 canaux), lignes contiguës.
#[derive(Clone, Debug, Default)]
pub struct Image {
    pub w: usize,
    pub h: usize,
    pub c: usize,
    pub data: Vec<u8>,
}

impl Image {
    pub fn new(w: usize, h: usize, c: usize) -> Self {
        Image { w, h, c, data: vec![0; w * h * c] }
    }

    pub fn from_bgr(w: usize, h: usize, data: Vec<u8>) -> Self {
        assert_eq!(data.len(), w * h * 3);
        Image { w, h, c: 3, data }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    #[inline]
    pub fn px(&self, x: usize, y: usize) -> &[u8] {
        let i = (y * self.w + x) * self.c;
        &self.data[i..i + self.c]
    }

    /// Copie de la zone [x0, x1) × [y0, y1) (bornée à l'image, comme un découpage NumPy).
    pub fn crop(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> Image {
        let (x0, y0) = (x0.clamp(0, self.w as i64) as usize, y0.clamp(0, self.h as i64) as usize);
        let (x1, y1) = (x1.clamp(0, self.w as i64) as usize, y1.clamp(0, self.h as i64) as usize);
        let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
        let mut out = Image::new(w, h, self.c);
        for r in 0..h {
            let s = ((y0 + r) * self.w + x0) * self.c;
            out.data[r * w * self.c..(r + 1) * w * self.c].copy_from_slice(&self.data[s..s + w * self.c]);
        }
        out
    }

    /// Recopie `src` à la position (x, y) (bornée).
    pub fn paste(&mut self, src: &Image, x: usize, y: usize) {
        let w = src.w.min(self.w.saturating_sub(x));
        for r in 0..src.h.min(self.h.saturating_sub(y)) {
            let d = ((y + r) * self.w + x) * self.c;
            self.data[d..d + w * self.c].copy_from_slice(&src.data[r * src.w * src.c..(r * src.w + w) * src.c]);
        }
    }

    /// cv2.copyMakeBorder(BORDER_CONSTANT) avec une même valeur sur tous les canaux.
    pub fn border(&self, top: usize, bottom: usize, left: usize, right: usize, value: u8) -> Image {
        let mut out = Image { w: self.w + left + right, h: self.h + top + bottom, c: self.c, data: vec![] };
        out.data = vec![value; out.w * out.h * out.c];
        out.paste(self, left, top);
        out
    }

    /// Image en canaux inversés (BGR ↔ RGB).
    pub fn swap_rb(&self) -> Image {
        let mut out = self.clone();
        for p in out.data.chunks_exact_mut(3) {
            p.swap(0, 2);
        }
        out
    }
}

const COEF_BITS: i32 = 11;
const COEF_SCALE: i32 = 1 << COEF_BITS;

/// Coefficients d'interpolation d'un axe (resizeGeneric_ d'OpenCV, mode non « area »).
/// Retourne (indice source, poids 0, poids 1) en virgule fixe, et la borne `xmax`.
fn linear_tab(src: usize, dst: usize) -> (Vec<usize>, Vec<[i16; 2]>, usize) {
    let inv_scale = dst as f64 / src as f64;
    let scale = 1.0 / inv_scale;
    let mut ofs = Vec::with_capacity(dst);
    let mut alpha = Vec::with_capacity(dst);
    let mut xmax = dst;
    for dx in 0..dst {
        let mut f = ((dx as f64 + 0.5) * scale - 0.5) as f32;
        let mut sx = f.floor() as i64;
        f -= sx as f32;
        if sx < 0 {
            f = 0.0;
            sx = 0;
        }
        if sx + 1 >= src as i64 {
            xmax = xmax.min(dx);
            if sx >= src as i64 - 1 {
                f = 0.0;
                sx = src as i64 - 1;
            }
        }
        ofs.push(sx as usize);
        let a0 = ((1.0 - f) * COEF_SCALE as f32).round_ties_even() as i16;
        let a1 = (f * COEF_SCALE as f32).round_ties_even() as i16;
        alpha.push([a0, a1]);
    }
    (ofs, alpha, xmax)
}

/// Poids verticaux (sans recalage des bords, comme OpenCV).
fn linear_tab_y(src: usize, dst: usize) -> Vec<(i64, [i16; 2])> {
    let scale = 1.0 / (dst as f64 / src as f64);
    (0..dst)
        .map(|dy| {
            let mut f = ((dy as f64 + 0.5) * scale - 0.5) as f32;
            let sy = f.floor() as i64;
            f -= sy as f32;
            (sy, [((1.0 - f) * COEF_SCALE as f32).round_ties_even() as i16, (f * COEF_SCALE as f32).round_ties_even() as i16])
        })
        .collect()
}

/// Nombre d'éléments traités par la boucle vectorielle de VResizeLinearVec_32s8u
/// (registres de 16 octets pour les roues pip d'OpenCV sur x86-64), le reste en scalaire.
fn vec_count(width: usize) -> usize {
    const U8: usize = 16;
    const I16: usize = 8;
    let mut x = 0;
    while x + U8 <= width {
        x += U8;
    }
    while x + I16 <= width {
        x += I16;
    }
    x
}

/// cv2.resize(INTER_LINEAR) sur 8 bits, bit à bit (y compris le cas ×2 → moyenne 2×2).
pub fn resize_linear(src: &Image, ow: usize, oh: usize) -> Image {
    if ow == src.w && oh == src.h {
        return src.clone();
    }
    if src.w == ow * 2 && src.h == oh * 2 {
        return resize_area2(src);
    }
    let cn = src.c;
    let (xofs, alpha, xmax) = linear_tab(src.w, ow);
    let ytab = linear_tab_y(src.h, oh);
    let width = ow * cn;
    let nvec = vec_count(width);
    // table par élément (pixel × canal) : deux indices source et deux poids ; au-delà de xmax,
    // OpenCV prend le pixel seul × 2048, ce qui revient à des poids (2048, 0)
    let mut tab: Vec<(u32, u32, i32, i32)> = Vec::with_capacity(width);
    for dx in 0..ow {
        let sx = xofs[dx] * cn;
        for k in 0..cn {
            tab.push(if dx < xmax {
                ((sx + k) as u32, (sx + k + cn) as u32, alpha[dx][0] as i32, alpha[dx][1] as i32)
            } else {
                ((sx + k) as u32, (sx + k) as u32, COEF_SCALE, 0)
            });
        }
    }
    let row_len = src.w * cn;
    let hrow = |sy: usize, d: &mut [i32]| {
        let s = &src.data[sy * row_len..(sy + 1) * row_len];
        for (o, &(i0, i1, a0, a1)) in d.iter_mut().zip(&tab) {
            *o = s[i0 as usize] as i32 * a0 + s[i1 as usize] as i32 * a1;
        }
    };
    let mut out = Image::new(ow, oh, cn);
    // deux lignes interpolées horizontalement, réutilisées d'une ligne cible à la suivante
    let (mut buf0, mut buf1) = (vec![0i32; width], vec![0i32; width]);
    let (mut have0, mut have1) = (usize::MAX, usize::MAX);
    for (dy, &(sy, beta)) in ytab.iter().enumerate() {
        let r0 = sy.clamp(0, src.h as i64 - 1) as usize;
        let r1 = (sy + 1).clamp(0, src.h as i64 - 1) as usize;
        if have0 != r0 {
            if have1 == r0 {
                std::mem::swap(&mut buf0, &mut buf1);
                std::mem::swap(&mut have0, &mut have1);
            } else {
                hrow(r0, &mut buf0);
                have0 = r0;
            }
        }
        if have1 != r1 {
            hrow(r1, &mut buf1);
            have1 = r1;
        }
        let (s0, s1) = (&buf0, &buf1);
        let (b0, b1) = (beta[0] as i32, beta[1] as i32);
        let d = &mut out.data[dy * width..(dy + 1) * width];
        for ((o, &a), &b) in d[..nvec].iter_mut().zip(&s0[..nvec]).zip(&s1[..nvec]) {
            // v_pack(v_shr<4>) sature en 16 bits, v_mul_hi garde les 16 bits hauts, v_rshr_pack_u<2>
            let a = (a >> 4).clamp(i16::MIN as i32, i16::MAX as i32);
            let b = (b >> 4).clamp(i16::MIN as i32, i16::MAX as i32);
            let v = ((a * b0) >> 16) + ((b * b1) >> 16);
            *o = ((v + 2) >> 2).clamp(0, 255) as u8;
        }
        for x in nvec..width {
            let v = s0[x] as i64 * b0 as i64 + s1[x] as i64 * b1 as i64;
            d[x] = ((v + (1 << 21)) >> 22).clamp(0, 255) as u8;
        }
    }
    out
}

/// cv2.resize(INTER_AREA) pour une réduction exacte de moitié : moyenne 2×2 arrondie.
pub fn resize_area2(src: &Image) -> Image {
    let (ow, oh, cn) = (src.w / 2, src.h / 2, src.c);
    let mut out = Image::new(ow, oh, cn);
    let row = src.w * cn;
    for (y, d) in out.data.chunks_exact_mut(ow * cn).enumerate() {
        let r0 = &src.data[2 * y * row..2 * y * row + 2 * ow * cn];
        let r1 = &src.data[(2 * y + 1) * row..(2 * y + 1) * row + 2 * ow * cn];
        for ((o, p0), p1) in d.chunks_exact_mut(cn).zip(r0.chunks_exact(2 * cn)).zip(r1.chunks_exact(2 * cn)) {
            for k in 0..cn {
                let s = p0[k] as u32 + p0[k + cn] as u32 + p1[k] as u32 + p1[k + cn] as u32;
                o[k] = ((s + 2) >> 2) as u8;
            }
        }
    }
    out
}

/// cv2.resize(INTER_AREA) : réduction de moitié exacte, sinon bilinéaire (usage : vignettes).
pub fn resize_area(src: &Image, ow: usize, oh: usize) -> Image {
    if src.w == ow * 2 && src.h == oh * 2 { resize_area2(src) } else { resize_linear(src, ow, oh) }
}

/// Masque « clair et peu coloré » : V > 150 et S < 70 (cv2.cvtColor BGR2HSV, 8 bits).
pub fn light_mask(img: &Image) -> Vec<bool> {
    const SHIFT: i32 = 12;
    let sdiv: Vec<i32> = (0..256)
        .map(|i| if i == 0 { 0 } else { ((255 << SHIFT) as f64 / i as f64).round_ties_even() as i32 })
        .collect();
    img.data
        .chunks_exact(3)
        .map(|p| {
            let v = p[0].max(p[1]).max(p[2]) as i32;
            let mn = p[0].min(p[1]).min(p[2]) as i32;
            let s = ((v - mn) * sdiv[v as usize] + (1 << (SHIFT - 1))) >> SHIFT;
            v > 150 && s < 70
        })
        .collect()
}

/// Composante connexe (8-connexité) : boîte et aire, comme connectedComponentsWithStats.
#[derive(Clone, Copy, Debug)]
pub struct Component {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub area: usize,
}

/// Composantes du masque, dans l'ordre des étiquettes d'OpenCV (parcours par blocs 2×2).
pub fn components(mask: &[bool], w: usize, h: usize) -> Vec<Component> {
    let mut label = vec![usize::MAX; w * h];
    let mut comps: Vec<(usize, usize, Component)> = vec![]; // (bloc y, bloc x, stats)
    let mut stack = vec![];
    for y in 0..h {
        for x in 0..w {
            if !mask[y * w + x] || label[y * w + x] != usize::MAX {
                continue;
            }
            let id = comps.len();
            let (mut x0, mut y0, mut x1, mut y1, mut area) = (x, y, x, y, 0);
            let mut first = (y / 2, x / 2);
            label[y * w + x] = id;
            stack.push((x, y));
            while let Some((cx, cy)) = stack.pop() {
                area += 1;
                x0 = x0.min(cx);
                x1 = x1.max(cx);
                y0 = y0.min(cy);
                y1 = y1.max(cy);
                first = first.min((cy / 2, cx / 2));
                for dy in -1i64..=1 {
                    for dx in -1i64..=1 {
                        let (nx, ny) = (cx as i64 + dx, cy as i64 + dy);
                        if nx < 0 || ny < 0 || nx >= w as i64 || ny >= h as i64 {
                            continue;
                        }
                        let i = ny as usize * w + nx as usize;
                        if mask[i] && label[i] == usize::MAX {
                            label[i] = id;
                            stack.push((nx as usize, ny as usize));
                        }
                    }
                }
            }
            comps.push((first.0, first.1, Component { x: x0, y: y0, w: x1 - x0 + 1, h: y1 - y0 + 1, area }));
        }
    }
    comps.sort_by_key(|c| (c.0, c.1));
    comps.into_iter().map(|c| c.2).collect()
}

/// Produit scalaire sur 8 accumulateurs (vectorisable par le compilateur).
fn dot8(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

/// cv2.matchTemplate(TM_CCOEFF_NORMED) puis minMaxLoc : (meilleur score, x, y).
/// Calcul direct (OpenCV passe par une FFT en float32, à ~1e-3 près : sur deux maxima
/// quasi égaux, la position retenue peut différer d'un pixel).
pub fn match_template_best(area: &Image, templ: &Image) -> Option<(f64, usize, usize)> {
    let (tw, th, cn) = (templ.w, templ.h, templ.c);
    if area.w < tw || area.h < th || tw == 0 || th == 0 {
        return None;
    }
    let n = (tw * th) as f64;
    let mut tmean = vec![0.0f64; cn];
    for p in templ.data.chunks_exact(cn) {
        for k in 0..cn {
            tmean[k] += p[k] as f64;
        }
    }
    tmean.iter_mut().for_each(|m| *m /= n);
    let tz: Vec<f32> = templ.data.iter().enumerate().map(|(i, &v)| (v as f64 - tmean[i % cn]) as f32).collect();
    let tnorm2: f64 = tz.iter().map(|v| (*v as f64) * (*v as f64)).sum();
    let (rw, rh) = (area.w - tw + 1, area.h - th + 1);
    if tnorm2.sqrt() < f64::EPSILON {
        return Some((1.0, 0, 0));
    }
    let tnorm = tnorm2.sqrt();
    // sommes glissantes de la fenêtre (intégrales) par canal
    let iw = area.w + 1;
    let mut s1 = vec![0.0f64; iw * (area.h + 1) * cn];
    let mut s2 = vec![0.0f64; iw * (area.h + 1) * cn];
    for y in 0..area.h {
        for x in 0..area.w {
            for k in 0..cn {
                let v = area.data[(y * area.w + x) * cn + k] as f64;
                let i = ((y + 1) * iw + x + 1) * cn + k;
                s1[i] = v + s1[i - cn] + s1[i - iw * cn] - s1[i - iw * cn - cn];
                s2[i] = v * v + s2[i - cn] + s2[i - iw * cn] - s2[i - iw * cn - cn];
            }
        }
    }
    let rect = |s: &[f64], x: usize, y: usize, k: usize| {
        s[((y + th) * iw + x + tw) * cn + k] - s[(y * iw + x + tw) * cn + k] - s[((y + th) * iw + x) * cn + k] + s[(y * iw + x) * cn + k]
    };
    let af: Vec<f32> = area.data.iter().map(|v| *v as f32).collect();
    // meilleur score d'une ligne du résultat ; lignes réparties sur les cœurs (calcul direct coûteux)
    let row = |y: usize| -> Option<(f64, usize, usize)> {
        let mut best: Option<(f64, usize, usize)> = None;
        for x in 0..rw {
            // corrélation avec le gabarit centré (la moyenne de la fenêtre s'annule : Σ tz = 0)
            let mut num = 0.0f64;
            for r in 0..th {
                let a = &af[((y + r) * area.w + x) * cn..((y + r) * area.w + x + tw) * cn];
                let t = &tz[r * tw * cn..(r + 1) * tw * cn];
                num += dot8(a, t) as f64;
            }
            let (mut sum2, mut mean2) = (0.0, 0.0);
            for k in 0..cn {
                let m = rect(&s1, x, y, k);
                sum2 += rect(&s2, x, y, k);
                mean2 += m * m / n;
            }
            let diff2 = (sum2 - mean2).max(0.0);
            let t = if diff2 <= (0.5f64).min(10.0 * f32::EPSILON as f64 * sum2) { 0.0 } else { diff2.sqrt() * tnorm };
            let v = if num.abs() < t {
                num / t
            } else if num.abs() < t * 1.125 {
                num.signum()
            } else {
                0.0
            };
            if best.is_none_or(|b| v > b.0) {
                best = Some((v, x, y));
            }
        }
        best
    };
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(rh).max(1);
    let per = rh.div_ceil(threads);
    let parts: Vec<Option<(f64, usize, usize)>> = std::thread::scope(|sc| {
        let handles: Vec<_> = (0..threads)
            .map(|k| {
                let row = &row;
                sc.spawn(move || {
                    let mut best: Option<(f64, usize, usize)> = None;
                    for y in k * per..((k + 1) * per).min(rh) {
                        if let Some(r) = row(y)
                            && best.is_none_or(|b| r.0 > b.0)
                        {
                            best = Some(r);
                        }
                    }
                    best
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(None)).collect()
    });
    // premier maximum dans l'ordre de balayage (comme minMaxLoc)
    let mut best: Option<(f64, usize, usize)> = None;
    for r in parts.into_iter().flatten() {
        if best.is_none_or(|b| r.0 > b.0) {
            best = Some(r);
        }
    }
    best
}

/// Noyau gaussien d'OpenCV (sigma déduit de la taille, comme GaussianBlur(k, 0)).
fn gauss_kernel(k: usize) -> Vec<f32> {
    // petites tailles : tables fixes d'OpenCV (getGaussianKernel, sigma ≤ 0)
    match k {
        1 => return vec![1.0],
        3 => return vec![0.25, 0.5, 0.25],
        5 => return vec![0.0625, 0.25, 0.375, 0.25, 0.0625],
        7 => return vec![0.03125, 0.109375, 0.21875, 0.28125, 0.21875, 0.109375, 0.03125],
        _ => {}
    }
    let sigma = 0.3 * ((k as f64 - 1.0) * 0.5 - 1.0) + 0.8;
    let c = (k as f64 - 1.0) / 2.0;
    let w: Vec<f64> = (0..k).map(|i| (-((i as f64 - c).powi(2)) / (2.0 * sigma * sigma)).exp()).collect();
    let s: f64 = w.iter().sum();
    w.iter().map(|v| (v / s) as f32).collect()
}

fn reflect101(i: i64, n: i64) -> usize {
    if n == 1 {
        return 0;
    }
    let mut i = i;
    loop {
        if i < 0 {
            i = -i;
        } else if i >= n {
            i = 2 * n - 2 - i;
        } else {
            return i as usize;
        }
    }
}

/// cv2.GaussianBlur(img, (k, k), 0) sur une image isolée (bords en miroir 101).
pub fn gaussian_blur(img: &Image, k: usize) -> Image {
    let kern = gauss_kernel(k);
    let r = (k / 2) as i64;
    let (w, h, cn) = (img.w, img.h, img.c);
    let mut tmp = vec![0f32; w * h * cn];
    for y in 0..h {
        for x in 0..w {
            for c in 0..cn {
                let mut s = 0.0;
                for (j, kv) in kern.iter().enumerate() {
                    let xx = reflect101(x as i64 + j as i64 - r, w as i64);
                    s += kv * img.data[(y * w + xx) * cn + c] as f32;
                }
                tmp[(y * w + x) * cn + c] = s;
            }
        }
    }
    let mut out = Image::new(w, h, cn);
    for y in 0..h {
        for x in 0..w {
            for c in 0..cn {
                let mut s = 0.0;
                for (j, kv) in kern.iter().enumerate() {
                    let yy = reflect101(y as i64 + j as i64 - r, h as i64);
                    s += kv * tmp[(yy * w + x) * cn + c];
                }
                out.data[(y * w + x) * cn + c] = s.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Blob NCHW flottant : (pixel − moyenne[c]) × échelle[c], canaux éventuellement inversés
/// (cv2.dnn.blobFromImage).
pub fn blob(img: &Image, mean: [f32; 3], scale: [f32; 3], swap_rb: bool) -> Vec<f32> {
    let plane = img.w * img.h;
    let mut out = vec![0f32; plane * 3];
    for (i, p) in img.data.chunks_exact(3).enumerate() {
        for k in 0..3 {
            let d = if swap_rb { 2 - k } else { k };
            out[d * plane + i] = (p[k] as f32 - mean[d]) * scale[d];
        }
    }
    out
}
