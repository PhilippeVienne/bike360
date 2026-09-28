// Analyse d'horizon sur GPU (portage de horizon.py) :
// 1. equirect : image équirectangulaire (luminance) depuis le .lrv double fisheye,
//    redressée de l'inclinaison fixe (d_cam = M·d_eq, même modèle que reproject.cu) ;
// 2. edges    : normale du grand cercle porté par chaque contour + poids ;
// 3. scores   : pour chaque orientation candidate g, Σ w·exp(−(g·n)²/σ²).

struct EqParams {
    const unsigned char* y;  // plan Y du .lrv (côte à côte : arrière à gauche, avant à droite)
    int in_w, in_h, pitch;
    float* out;              // W × H flottants
    int W, H;
    float m[9];              // repère image équirect → repère caméra
    float lens_half;         // demi-champ d'un objectif (radians)
};

__device__ float lum(const EqParams& P, float u, float v, float s) {
    // u, v ∈ [0, 1] dans l'image d'un objectif ; s > 0 : avant (moitié droite)
    float lw = P.in_w * 0.5f;
    float x = (s > 0 ? lw : 0.f) + u * lw - 0.5f, y = v * P.in_h - 0.5f;
    x = fminf(fmaxf(x, 0.f), P.in_w - 1.001f);
    y = fminf(fmaxf(y, 0.f), P.in_h - 1.001f);
    int x0 = (int)x, y0 = (int)y;
    float fx = x - x0, fy = y - y0;
    const unsigned char* r = P.y + y0 * P.pitch + x0;
    return (r[0] * (1 - fx) + r[1] * fx) * (1 - fy) + (r[P.pitch] * (1 - fx) + r[P.pitch + 1] * fx) * fy;
}

__device__ bool lens_uv(const EqParams& P, float dx, float dy, float dz, float s, float& u, float& v) {
    float z = dz * s, x = dx * s;
    float rn = acosf(fminf(fmaxf(z, -1.f), 1.f)) / P.lens_half;
    if (rn > 1.02f) return false;
    float h = sqrtf(x * x + dy * dy);
    float cx = h > 1e-6f ? x / h : 0.f, cy = h > 1e-6f ? dy / h : 0.f;
    u = 0.5f + cx * 0.5f * rn;
    v = 0.5f - cy * 0.5f * rn;
    return true;
}

extern "C" __global__ void equirect(EqParams P) {
    int i = blockIdx.x * blockDim.x + threadIdx.x, j = blockIdx.y * blockDim.y + threadIdx.y;
    if (i >= P.W || j >= P.H) return;
    float lon = ((i + 0.5f) / P.W) * 6.2831853f - 3.1415927f;
    float lat = 1.5707963f - ((j + 0.5f) / P.H) * 3.1415927f;
    float ex = cosf(lat) * sinf(lon), ey = sinf(lat), ez = cosf(lat) * cosf(lon);
    const float* m = P.m;
    float dx = m[0] * ex + m[1] * ey + m[2] * ez;
    float dy = m[3] * ex + m[4] * ey + m[5] * ez;
    float dz = m[6] * ex + m[7] * ey + m[8] * ez;
    float w = fminf(fmaxf((dz + 0.04f) / 0.08f, 0.f), 1.f);
    w = w * w * (3.f - 2.f * w);
    float u, v, a = 0.f, b = 0.f;
    if (w > 0.f && lens_uv(P, dx, dy, dz, 1.f, u, v)) a = lum(P, u, v, 1.f);
    if (w < 1.f && lens_uv(P, dx, dy, dz, -1.f, u, v)) b = lum(P, u, v, -1.f);
    P.out[j * P.W + i] = a * w + b * (1.f - w);
}

struct EdgeParams {
    const float* img;
    int W, H;
    float4* out;             // (W/2) × (H/2) : normale xyz + poids (0 = exclu)
    float lat_min, lat_max;  // degrés
    float excl_lon0, excl_lon1;
    float grad_min, weight_cap;
};

extern "C" __global__ void edges(EdgeParams P) {
    int i2 = blockIdx.x * blockDim.x + threadIdx.x, j2 = blockIdx.y * blockDim.y + threadIdx.y;
    int W2 = P.W / 2, H2 = P.H / 2;
    if (i2 >= W2 || j2 >= H2) return;
    int i = 2 * i2 + 1, j = 2 * j2 + 1;      // mêmes pixels que keep[::2]=False en numpy (indices impairs)
    float4 r = make_float4(0.f, 0.f, 0.f, 0.f);
    if (i < P.W - 1 && j < P.H - 1) {
        const float* im = P.img;
        float gx = (im[j * P.W + i + 1] - im[j * P.W + i - 1]) * 0.5f;
        float gy = (im[(j + 1) * P.W + i] - im[(j - 1) * P.W + i]) * 0.5f;
        float mag = sqrtf(gx * gx + gy * gy);
        float lon = ((i + 0.5f) / P.W) * 6.2831853f - 3.1415927f;
        float lat = 1.5707963f - ((j + 0.5f) / P.H) * 3.1415927f;
        float lat_d = lat * 57.29578f, lon_d = fmodf(lon * 57.29578f + 360.f, 360.f);
        bool keep = mag > P.grad_min && lat_d > P.lat_min && lat_d < P.lat_max
                    && !(lon_d > P.excl_lon0 && lon_d < P.excl_lon1);
        if (keep) {
            float cl = cosf(lat), sl = sinf(lat), cn = cosf(lon), sn = sinf(lon);
            float d[3] = {cl * sn, sl, cl * cn};
            float el[3] = {cn, 0.f, -sn};
            float ea[3] = {-sl * sn, cl, -sl * cn};
            float t[3];
            for (int k = 0; k < 3; k++) t[k] = -gy * el[k] - gx * ea[k];
            float n[3] = {d[1] * t[2] - d[2] * t[1], d[2] * t[0] - d[0] * t[2], d[0] * t[1] - d[1] * t[0]};
            float nn = sqrtf(n[0] * n[0] + n[1] * n[1] + n[2] * n[2]) + 1e-9f;
            r = make_float4(n[0] / nn, n[1] / nn, n[2] / nn, fminf(mag, P.weight_cap));
        }
    }
    P.out[j2 * W2 + i2] = r;
}

// Un bloc par orientation candidate ; réduction en mémoire partagée.
extern "C" __global__ void scores(const float4* e, int n, const float* states, float inv_s2, float* out) {
    __shared__ float acc[256];
    int s = blockIdx.x;
    float gx = states[3 * s], gy = states[3 * s + 1], gz = states[3 * s + 2];
    float sum = 0.f;
    for (int k = threadIdx.x; k < n; k += blockDim.x) {
        float4 v = e[k];
        if (v.w > 0.f) {
            float d = gx * v.x + gy * v.y + gz * v.z;
            sum += v.w * __expf(-d * d * inv_s2);
        }
    }
    acc[threadIdx.x] = sum;
    __syncthreads();
    for (int st = blockDim.x / 2; st > 0; st >>= 1) {
        if (threadIdx.x < st) acc[threadIdx.x] += acc[threadIdx.x + st];
        __syncthreads();
    }
    if (threadIdx.x == 0) out[s] = acc[0];
}
