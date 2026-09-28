// Reprojection double fisheye (Insta360 X5) → vue plane, en NV12, entièrement sur GPU.
//
// Même modèle que la visionneuse WebGL et que ffmpeg v360 (dfisheye, équidistant) :
// un rayon d'écran d devient d_cam = M·d (M fournie par l'appelant : vue + horizon +
// inclinaison de la caméra) ; z > 0 → objectif avant, z < 0 → objectif arrière (miroir).
// Chaque thread produit un bloc 2×2 : 4 échantillons de luminance, 1 de chrominance.

struct Lens {
    const unsigned char* y;   // plan Y (w × h, pas = w)
    const unsigned char* uv;  // plan UV entrelacé (w × h/2)
};

struct Params {
    Lens front, back;
    int in_w, in_h;
    unsigned char* out_y;
    unsigned char* out_uv;
    int out_w, out_h, out_pitch;
    float m[9];               // rotation écran → caméra (ligne par ligne)
    float tan_h, tan_v;       // tan(champ/2) horizontal et vertical
    float lens_half;          // demi-champ d'un objectif (radians)
    const float* masks;       // n × (x, y, w, h) en coordonnées de l'image côte à côte (0..1)
    int n_masks;
};

__device__ float bilinear(const unsigned char* p, int w, int h, int stride, int channel, float x, float y) {
    x = fminf(fmaxf(x - 0.5f, 0.f), w - 1.001f);
    y = fminf(fmaxf(y - 0.5f, 0.f), h - 1.001f);
    int x0 = (int)x, y0 = (int)y;
    float fx = x - x0, fy = y - y0;
    const unsigned char* r0 = p + y0 * stride * 1 + x0 * (channel < 0 ? 1 : 2) + (channel < 0 ? 0 : channel);
    int step = channel < 0 ? 1 : 2;
    float a = r0[0], b = r0[step], c = r0[stride], d = r0[stride + step];
    return (a * (1 - fx) + b * fx) * (1 - fy) + (c * (1 - fx) + d * fx) * fy;
}

// Coordonnées normalisées (0..1) dans l'image d'un objectif pour le rayon d.
__device__ bool lens_uv(const Params& P, float dx, float dy, float dz, float s, float& u, float& v) {
    float z = dz * s, x = dx * s;
    float rn = acosf(fminf(fmaxf(z, -1.f), 1.f)) / P.lens_half;
    if (rn > 1.02f) return false;
    float h = sqrtf(x * x + dy * dy);
    float cx = h > 1e-6f ? x / h : 0.f, cy = h > 1e-6f ? dy / h : 0.f;
    u = 0.5f + cx * 0.5f * rn;
    v = 0.5f - cy * 0.5f * rn;
    return true;
}

__device__ bool masked(const Params& P, float u, float v, float s) {
    float gx = s > 0 ? 0.5f + u * 0.5f : u * 0.5f;  // position dans l'image côte à côte
    for (int i = 0; i < P.n_masks; i++) {
        const float* m = P.masks + 4 * i;
        if (gx > m[0] && gx < m[0] + m[2] && v > m[1] && v < m[1] + m[3]) return true;
    }
    return false;
}

// Échantillonne un canal d'un objectif (channel -1 = Y, 0 = U, 1 = V), avec floutage des masques.
__device__ float sample(const Params& P, float u, float v, float s, int channel) {
    const Lens& L = s > 0 ? P.front : P.back;
    bool chroma = channel >= 0;
    int w = chroma ? P.in_w / 2 : P.in_w, h = chroma ? P.in_h / 2 : P.in_h;
    const unsigned char* plane = chroma ? L.uv : L.y;
    if (P.n_masks && masked(P, u, v, s)) {
        float acc = 0;
        for (int a = -3; a <= 3; a++)
            for (int b = -3; b <= 3; b++)
                acc += bilinear(plane, w, h, P.in_w, channel, (u + a * 0.014f) * w, (v + b * 0.014f) * h);
        return acc / 49.f;
    }
    return bilinear(plane, w, h, P.in_w, channel, u * w, v * h);
}

__device__ float value_at(const Params& P, float px, float py, int channel) {
    // Rayon d'écran puis rotation vers le repère caméra.
    float sx = px * P.tan_h, sy = py * P.tan_v, sz = 1.f;
    float n = rsqrtf(sx * sx + sy * sy + 1.f);
    sx *= n; sy *= n; sz *= n;
    const float* m = P.m;
    float dx = m[0] * sx + m[1] * sy + m[2] * sz;
    float dy = m[3] * sx + m[4] * sy + m[5] * sz;
    float dz = m[6] * sx + m[7] * sy + m[8] * sz;
    float w = fminf(fmaxf((dz + 0.04f) / 0.08f, 0.f), 1.f);
    w = w * w * (3.f - 2.f * w);  // smoothstep : fondu à la couture
    float u, v, a = 0.f, b = 0.f;
    if (w > 0.f && lens_uv(P, dx, dy, dz, 1.f, u, v)) a = sample(P, u, v, 1.f, channel);
    if (w < 1.f && lens_uv(P, dx, dy, dz, -1.f, u, v)) b = sample(P, u, v, -1.f, channel);
    return a * w + b * (1.f - w);
}

extern "C" __global__ void reproject(Params P) {
    int bx = blockIdx.x * blockDim.x + threadIdx.x;
    int by = blockIdx.y * blockDim.y + threadIdx.y;
    if (bx * 2 >= P.out_w || by * 2 >= P.out_h) return;
    float inv_w = 2.f / P.out_w, inv_h = 2.f / P.out_h;
    for (int j = 0; j < 2; j++)
        for (int i = 0; i < 2; i++) {
            int x = bx * 2 + i, y = by * 2 + j;
            float Y = value_at(P, (x + 0.5f) * inv_w - 1.f, 1.f - (y + 0.5f) * inv_h, -1);
            P.out_y[y * P.out_pitch + x] = (unsigned char)fminf(fmaxf(16.f + Y * (219.f / 255.f) + 0.5f, 0.f), 255.f);
        }
    float cx = (bx * 2 + 1.f) * inv_w - 1.f, cy = 1.f - (by * 2 + 1.f) * inv_h;
    for (int c = 0; c < 2; c++) {
        float C = value_at(P, cx, cy, c);
        P.out_uv[by * P.out_pitch + bx * 2 + c] =
            (unsigned char)fminf(fmaxf(128.f + (C - 128.f) * (224.f / 255.f) + 0.5f, 0.f), 255.f);
    }
}
