// Géométrie de la vue : redressement de l'horizon, points clés de cadrage, projection des
// zones de confidentialité. Mêmes formules que core/src/geometry.rs (et geometry.py /
// privacy.py) : l'aperçu doit montrer exactement ce que l'export produira.
// Matrices 3×3 stockées par lignes dans des tableaux de 9 nombres.

import { st } from "./state.js";

const IDENTITY = [1, 0, 0, 0, 1, 0, 0, 0, 1];

// ------------------------------------------------------------------ matrices

/** Rotation de deg degrés autour de l'axe « p » (tangage), « y » (lacet) ou « r » (roulis). */
function rotRows(axis, deg) {
  const a = deg * Math.PI / 180, c = Math.cos(a), s = Math.sin(a);
  return { p: [1, 0, 0, 0, c, s, 0, -s, c], y: [c, 0, s, 0, 1, 0, -s, 0, c], r: [c, -s, 0, s, c, 0, 0, 0, 1] }[axis];
}
function mul3(a, b) {
  const o = new Array(9).fill(0);
  for (let r = 0; r < 3; r++) for (let c = 0; c < 3; c++) for (let k = 0; k < 3; k++) o[r * 3 + c] += a[r * 3 + k] * b[k * 3 + c];
  return o;
}
/** Rotation minimale qui amène l'axe y sur la verticale mesurée u. */
function minRotation(u) {
  const n = Math.hypot(u[0], u[1], u[2]); const [x, y, z] = [u[0] / n, u[1] / n, u[2] / n];
  const ax = [z, 0, -x], s = Math.hypot(ax[0], ax[2]), c = y;
  if (s < 1e-9) return IDENTITY;
  const k = [ax[0] / s, 0, ax[2] / s];
  const K = [0, -k[2], k[1], k[2], 0, -k[0], -k[1], k[0], 0], K2 = mul3(K, K);
  return K.map((v, i) => (i % 4 === 0 ? 1 : 0) + s * v + (1 - c) * K2[i]);
}

// ------------------------------------------------------------------ horizon

/** Mode d'horizon d'un clip : auto, fixe ou aucun (cf. geometry.clip_horizon_mode). */
export function clipMode(c) {
  return ["auto", "fixe", "aucun"].includes(c.horizon) ? c.horizon : (c.level ? "auto" : "aucun");
}

/** Matrice de redressement à l'instant t : horizon mesuré (auto), inclinaison du support (fixe) ou rien. */
export function levelMatrix(t, mode) {
  if (mode === "aucun") return IDENTITY;
  const h = st.horizon;
  if (mode === "auto" && h && h.up) {
    const x = Math.max(0, Math.min(h.up.length - 1.001, t * h.hz)), i = Math.floor(x), f = x - i;
    const a = h.up[i], b = h.up[i + 1];
    return minRotation([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f]);
  }
  const tl = st.s.tilt || { pitch: 0, roll: 0 };
  return mul3(rotRows("p", tl.pitch), rotRows("r", tl.roll));
}

/** Matrice écran → caméra d'une vue {yaw, pitch, roll} redressée par L (geometry.view_matrix). */
export function viewMatrix(L, v) {
  return mul3(L, mul3(mul3(rotRows("y", v.yaw), rotRows("p", v.pitch)), rotRows("r", v.roll || 0)));
}

// ------------------------------------------------------------------ points clés de cadrage (geometry.clip_view_at)

const EASINGS = {
  linear: (u) => u,
  ease_in_out: (u) => u * u * (3 - 2 * u),
  ease_in: (u) => u * u,
  ease_out: (u) => 1 - (1 - u) ** 2,
  quick: (u) => u ** 3 * (u * (6 * u - 15) + 10),
  delay: (u) => (u < 0.5 ? 0 : ((u - 0.5) * 2) ** 2 * (3 - 4 * (u - 0.5))),
  cut: (u) => (u < 1 ? 0 : 1),
};
export const CURVE_LABELS = { linear: "linéaire", ease_in_out: "douce", ease_in: "douce à l'entrée", ease_out: "douce à la sortie",
                              quick: "rapide", delay: "départ retardé", cut: "coupe franche" };

function baseView(c) { return { yaw: c.yaw ?? 0, pitch: c.pitch ?? 0, roll: c.roll ?? 0, fov: c.fov ?? 100 }; }

/** Points clés d'un clip, triés (les anciens « roll_keys » sont convertis à la volée). */
export function clipKeys(c) {
  if (c.keyframes?.length) return [...c.keyframes].sort((a, b) => a.t - b.t);
  if (c.roll_keys?.length) return c.roll_keys.map(([t, r]) => ({ ...baseView(c), t, roll: r, curve: "linear" }));
  return [];
}

/** Convertit pour de bon les anciens « roll_keys » d'un clip en points clés modifiables. */
export function ensureKeyframes(c) {
  if (c.roll_keys) { c.keyframes = clipKeys(c); delete c.roll_keys; }
}

/** Instant relatif au début du clip, borné à sa durée, au centième. */
export const clipTime = (c, t) => +Math.max(0, Math.min(c.end - c.start, t - c.start)).toFixed(2);

/** Cadrage interpolé du clip à l'instant tr (relatif à son début). */
export function clipViewAt(c, tr) {
  const k = clipKeys(c);
  if (!k.length) return baseView(c);
  const pick = (x) => ({ yaw: x.yaw, pitch: x.pitch, roll: x.roll, fov: x.fov });
  if (tr <= k[0].t) return pick(k[0]);
  if (tr >= k[k.length - 1].t) return pick(k[k.length - 1]);
  const i = k.findIndex((x) => x.t > tr), a = k[i - 1], b = k[i];
  const u = (EASINGS[a.curve] || EASINGS.linear)((tr - a.t) / (b.t - a.t));
  const dyaw = ((b.yaw - a.yaw + 540) % 360) - 180;
  return { yaw: ((a.yaw + dyaw * u + 540) % 360) - 180, pitch: a.pitch + (b.pitch - a.pitch) * u,
           roll: a.roll + (b.roll - a.roll) * u, fov: a.fov + (b.fov - a.fov) * u };
}

// ------------------------------------------------------------------ zones de confidentialité (même calcul que privacy.py)

const PV_EXTEND = 0.25, PV_GAP = 0.8, PV_PAD = 0.3;

/** Zones suivies présentes à l'instant t : direction d sur la sphère et demi-angles ax, ay. */
export function regionsAt(tracks, t) {
  const out = [];
  for (const tr of tracks) {
    const s = tr.samples;
    if (!s || !s.length || t < s[0][0] - PV_EXTEND || t > s[s.length - 1][0] + PV_EXTEND) continue;
    const k = s.findIndex((x) => x[0] >= t);
    let d, ax, ay;
    if (k <= 0) { const a = k === 0 ? s[0] : s[s.length - 1]; d = a.slice(1, 4); ax = a[4]; ay = a[5]; }
    else {
      const a = s[k - 1], b = s[k];
      if (b[0] - a[0] > PV_GAP) {   // trou tenu seulement si l'objet est resté dans la même direction
        const dot = a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
        if (b[0] - a[0] > 3 || Math.acos(Math.max(-1, Math.min(1, dot))) * 180 / Math.PI > 5) continue;
      }
      const f = (t - a[0]) / Math.max(b[0] - a[0], 1e-6);
      d = [0, 1, 2].map((i) => a[1 + i] * (1 - f) + b[1 + i] * f);
      ax = a[4] * (1 - f) + b[4] * f; ay = a[5] * (1 - f) + b[5] * f;
    }
    out.push({ d, ax, ay, enabled: tr.enabled !== false });
  }
  return out;
}

/** Rectangle écran [x, y, w, h] (px) d'une zone vue par la matrice M, ou null si derrière. */
export function sphereToBox(d, ax, ay, M, hfov, W, H) {
  const v = [0, 1, 2].map((i) => M[i] * d[0] + M[3 + i] * d[1] + M[6 + i] * d[2]);   // Mᵀ·d
  if (v[2] <= 0.05) return null;
  const th = Math.tan(hfov * Math.PI / 360), tv = th * H / W;
  const u = (v[0] / v[2] / th + 1) * W / 2, y = (1 - v[1] / v[2] / tv) * H / 2;
  const r2 = (v[0] / v[2]) ** 2 + (v[1] / v[2]) ** 2, k = 1 + r2;
  const hw = Math.tan(ax) * k / th * W / 2 * (1 + PV_PAD), hh = Math.tan(ay) * k / tv * H / 2 * (1 + PV_PAD);
  return [u - hw, y - hh, 2 * hw, 2 * hh];
}
