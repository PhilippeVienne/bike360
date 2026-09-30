// Effets après reprojection, sur l'image NV12 de sortie (plage limitée BT.709), avant NVENC :
//  - floutage de zones (confidentialité) : moyenne par cellules puis reconstruction bilinéaire,
//    soit un flou fort et doux en deux passes, sans tampon plein format ;
//  - incrustation d'images RGBA (télémétrie) avec transparence par image.

struct Frame {
    unsigned char* y;   // plan Y
    unsigned char* uv;  // plan UV entrelacé (demi-résolution)
    int w, h, pitch;
};

// ---------------------------------------------------------------- floutage

struct Blur {
    int x, y, w, h;     // zone traitée (pixels, bornée à l'image, coordonnées paires) : zone + fondu
    int cell;           // taille d'une cellule (pixels de luminance, pair)
    int gw, gh;         // grille de cellules
    float* cells;       // gw × gh × 3 (Y, U, V moyens)
    float ix0, iy0, ix1, iy1;   // zone à couvrir entièrement (flou complet)
    float feather;              // largeur du fondu autour (pixels) : pas de rectangle net
};

// Poids du flou : 1 dans la zone, décroissance douce jusqu'à 0 à `feather` pixels (coins arrondis).
__device__ float blur_weight(const Blur& B, float px, float py) {
    float dx = fmaxf(fmaxf(B.ix0 - px, px - B.ix1), 0.f);
    float dy = fmaxf(fmaxf(B.iy0 - py, py - B.iy1), 0.f);
    float t = fminf(sqrtf(dx * dx + dy * dy) / fmaxf(B.feather, 1.f), 1.f);
    return 1.f - t * t * (3.f - 2.f * t);
}

// Passe 1 : un fil par cellule, moyenne de Y (pleine résolution) et de U, V (demi-résolution).
extern "C" __global__ void blur_cells(Frame F, Blur B) {
    int cx = blockIdx.x * blockDim.x + threadIdx.x, cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx >= B.gw || cy >= B.gh) return;
    int x0 = B.x + cx * B.cell, y0 = B.y + cy * B.cell;
    int x1 = min(x0 + B.cell, B.x + B.w), y1 = min(y0 + B.cell, B.y + B.h);
    float sy = 0.f, su = 0.f, sv = 0.f;
    int ny = 0, nc = 0;
    for (int yy = y0; yy < y1; yy++)
        for (int xx = x0; xx < x1; xx++) { sy += F.y[yy * F.pitch + xx]; ny++; }
    for (int yy = y0 / 2; yy < (y1 + 1) / 2; yy++)
        for (int xx = x0 / 2; xx < (x1 + 1) / 2; xx++) {
            const unsigned char* p = F.uv + yy * F.pitch + 2 * xx;
            su += p[0]; sv += p[1]; nc++;
        }
    float* o = B.cells + 3 * (cy * B.gw + cx);
    o[0] = ny ? sy / ny : 16.f;
    o[1] = nc ? su / nc : 128.f;
    o[2] = nc ? sv / nc : 128.f;
}

__device__ float cell_at(const Blur& B, float fx, float fy, int c) {
    // centre des cellules en (i + 0.5) × cell ; interpolation bilinéaire bornée à la grille
    float gx = fminf(fmaxf(fx / B.cell - 0.5f, 0.f), B.gw - 1.001f);
    float gy = fminf(fmaxf(fy / B.cell - 0.5f, 0.f), B.gh - 1.001f);
    int ix = (int)gx, iy = (int)gy;
    int jx = min(ix + 1, B.gw - 1), jy = min(iy + 1, B.gh - 1);
    float ax = gx - ix, ay = gy - iy;
    float a = B.cells[3 * (iy * B.gw + ix) + c], b = B.cells[3 * (iy * B.gw + jx) + c];
    float d = B.cells[3 * (jy * B.gw + ix) + c], e = B.cells[3 * (jy * B.gw + jx) + c];
    return (a * (1 - ax) + b * ax) * (1 - ay) + (d * (1 - ax) + e * ax) * ay;
}

// Passe 2 : un fil par bloc 2×2 de la zone ; les valeurs lissées remplacent l'image.
extern "C" __global__ void blur_apply(Frame F, Blur B) {
    int bx = blockIdx.x * blockDim.x + threadIdx.x, by = blockIdx.y * blockDim.y + threadIdx.y;
    int x = B.x + 2 * bx, y = B.y + 2 * by;
    if (x >= B.x + B.w || y >= B.y + B.h) return;
    for (int dy = 0; dy < 2; dy++)
        for (int dx = 0; dx < 2; dx++) {
            int px = x + dx, py = y + dy;
            if (px < B.x + B.w && py < B.y + B.h) {
                float w = blur_weight(B, px + 0.5f, py + 0.5f);
                unsigned char* o = F.y + py * F.pitch + px;
                *o = (unsigned char)(*o * (1.f - w) + cell_at(B, px - B.x + 0.5f, py - B.y + 0.5f, 0) * w + 0.5f);
            }
        }
    float w = blur_weight(B, x + 1.f, y + 1.f);
    unsigned char* p = F.uv + (y / 2) * F.pitch + 2 * (x / 2);
    p[0] = (unsigned char)(p[0] * (1.f - w) + cell_at(B, x - B.x + 1.f, y - B.y + 1.f, 1) * w + 0.5f);
    p[1] = (unsigned char)(p[1] * (1.f - w) + cell_at(B, x - B.x + 1.f, y - B.y + 1.f, 2) * w + 0.5f);
}

// ---------------------------------------------------------------- incrustation d'images

struct Sprite {
    const unsigned char* rgba;   // w × h × 4, non prémultiplié
    int w, h;
    int x, y;                    // position dans l'image (peut déborder)
    float alpha;                 // opacité globale (fondus)
};

// Un fil par bloc 2×2 de l'image couverte par le sprite ; RGB → YUV BT.709 plage limitée.
extern "C" __global__ void composite(Frame F, Sprite S) {
    int bx = blockIdx.x * blockDim.x + threadIdx.x, by = blockIdx.y * blockDim.y + threadIdx.y;
    int x = (S.x & ~1) + 2 * bx, y = (S.y & ~1) + 2 * by;
    if (x >= F.w || y >= F.h || x > S.x + S.w || y > S.y + S.h) return;
    float asum = 0.f, us = 0.f, vs = 0.f;
    for (int dy = 0; dy < 2; dy++)
        for (int dx = 0; dx < 2; dx++) {
            int px = x + dx, py = y + dy, sx = px - S.x, sy = py - S.y;
            if (px < 0 || py < 0 || px >= F.w || py >= F.h || sx < 0 || sy < 0 || sx >= S.w || sy >= S.h) continue;
            const unsigned char* c = S.rgba + 4 * (sy * S.w + sx);
            float a = c[3] / 255.f * S.alpha;
            if (a <= 0.f) continue;
            float r = c[0] / 255.f, g = c[1] / 255.f, b = c[2] / 255.f;
            float Y = 16.f + 219.f * (0.2126f * r + 0.7152f * g + 0.0722f * b);
            unsigned char* o = F.y + py * F.pitch + px;
            *o = (unsigned char)(*o * (1.f - a) + Y * a + 0.5f);
            asum += a;
            us += a * (128.f + 224.f * (-0.1146f * r - 0.3854f * g + 0.5f * b));
            vs += a * (128.f + 224.f * (0.5f * r - 0.4542f * g - 0.0458f * b));
        }
    if (asum <= 0.f || x < 0 || y < 0) return;
    float a = asum / 4.f;   // couverture moyenne du bloc pour la chrominance
    unsigned char* p = F.uv + (y / 2) * F.pitch + 2 * (x / 2);
    p[0] = (unsigned char)(p[0] * (1.f - a) + (us / asum) * a + 0.5f);
    p[1] = (unsigned char)(p[1] * (1.f - a) + (vs / asum) * a + 0.5f);
}
