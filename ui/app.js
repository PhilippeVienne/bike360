"use strict";
// Outil de tri des balades Insta360 X5 : lecture des proxys .lrv (double fisheye),
// recadrage 360 en WebGL, timeline des métriques IMU/GPS, carte, sélection de clips.

const $ = (s) => document.querySelector(s);
const video = $("#video");
const LENS_FOV = 195;             // doit correspondre à server.py
const RATES = [1, 2, 4, 8, 16];
const SKIM_THRESHOLD = 0.45;      // score sous lequel le survol accélère


const st = {
  s: null,                        // session courante (JSON d'analyse)
  seg: -1,                        // index du segment chargé dans <video>
  pendingSeek: null,
  clips: [],
  sel: null,                      // index du clip sélectionné (éditeur)
  checked: new Set(),             // clips cochés (suppression en masse)
  loop: false,                    // lecture en boucle du clip sélectionné
  inPoint: null,
  rate: 1,
  skim: false,
  view: { yaw: 0, pitch: -10, fov: 100, roll: 0, raw: false, horizon: "fixe" },
  viewOverride: false,            // vue modifiée à la main : ne suit plus les points clés
  autoKey: false,                 // « Auto » : chaque changement de vue enregistre un point   // fixe : prévisible, garde la prise d'angle
  masks: [],                      // zones floutées {x,y,w,h} en coordonnées du .lrv (0..1)
  maskEdit: false,
  maskDraft: null,
  tl: { v0: 0, v1: 1 },           // fenêtre visible de la timeline (secondes)
  hover: null,
};

// ------------------------------------------------------------------ utilitaires

const fmt = (t) => {
  t = Math.max(0, Math.floor(t));
  const h = Math.floor(t / 3600), m = Math.floor((t % 3600) / 60), s = t % 60;
  return (h ? h + ":" + String(m).padStart(2, "0") : m) + ":" + String(s).padStart(2, "0");
};
const localClock = (t) => {
  const s0 = +st.s.time.slice(0, 2) * 3600 + +st.s.time.slice(2, 4) * 60 + +st.s.time.slice(4, 6);
  const x = Math.floor(s0 + t) % 86400;
  return [x / 3600 | 0, (x % 3600) / 60 | 0, x % 60].map((v) => String(v).padStart(2, "0")).join(":");
};
const api = async (method, url, body) => {
  const r = await fetch(url, { method, headers: { "Content-Type": "application/json" },
                               body: body === undefined ? undefined : JSON.stringify(body) });
  if (!r.ok) throw new Error(`${method} ${url} → ${r.status}`);
  return r.json();
};
const series = (k, t) => st.s.series[k][Math.max(0, Math.min(st.s.duration - 1, Math.floor(t)))];

// ------------------------------------------------------------------ lecture multi-segments

function now() {
  if (!st.s || st.seg < 0) return 0;
  return st.s.segments[st.seg].offset + video.currentTime;
}

function segmentAt(t) {
  const segs = st.s.segments;
  for (let i = segs.length - 1; i >= 0; i--) if (t >= segs[i].offset) return i;
  return 0;
}

function seek(t, play) {
  st.viewOverride = false;   // se déplacer = revenir au cadrage des points clés
  t = Math.max(0, Math.min(st.s.duration - 0.1, t));
  const i = segmentAt(t);
  const local = t - st.s.segments[i].offset;
  const wasPlaying = play ?? !video.paused;
  if (i !== st.seg) {
    st.seg = i;
    st.pendingSeek = { local, play: wasPlaying };
    video.src = "/media/" + encodeURIComponent(st.s.segments[i].lrv);
  } else {
    video.currentTime = local;
    if (wasPlaying) video.play();
  }
  followTimeline(t);
}

video.addEventListener("loadedmetadata", () => {
  if (!st.pendingSeek) return;
  video.currentTime = st.pendingSeek.local;
  if (st.pendingSeek.play) video.play();
  st.pendingSeek = null;
  applyRate();
});
// état de la vidéo affiché sur la scène (chargement lent depuis la carte SD, erreur…)
function stageMsg(html, error) {
  const el = $("#stage-msg");
  el.hidden = !html;
  el.classList.toggle("error", !!error);
  if (html) el.innerHTML = html;
}
video.addEventListener("loadstart", () => stageMsg('<span><span class="spin"></span>Chargement de la vidéo…</span>'));
video.addEventListener("waiting", () => { if (video.readyState < 3) stageMsg('<span><span class="spin"></span>Lecture en attente de données…</span>'); });
for (const ev of ["loadeddata", "canplay", "playing", "seeked"]) video.addEventListener(ev, () => stageMsg(null));
video.addEventListener("error", () => stageMsg(`Vidéo illisible (${video.error?.message || "code " + video.error?.code}).<br>
  La carte SD est-elle toujours montée ?`, true));
video.addEventListener("ended", () => {
  if (st.seg < st.s.segments.length - 1) seek(st.s.segments[st.seg + 1].offset + 0.01, true);
});
video.addEventListener("play", () => { $("#play").textContent = "❚❚"; if (!st.autoKey) st.viewOverride = false; });
video.addEventListener("pause", () => ($("#play").textContent = "▶"));

function togglePlay() { video.paused ? video.play() : video.pause(); }

function applyRate() {
  let r = st.rate;
  if (st.skim && st.s && series("score", now()) < SKIM_THRESHOLD) r = 16;
  if (video.playbackRate !== r) video.playbackRate = r;
}

function setRate(r) {
  st.rate = r;
  document.querySelectorAll("#rates button").forEach((b) => b.classList.toggle("active", +b.dataset.r === r));
  applyRate();
}

// ------------------------------------------------------------------ visionneuse WebGL (double fisheye → vue plane)

const gl = $("#gl").getContext("webgl", { antialias: false });
const VS = `attribute vec2 p; varying vec2 uv; void main(){ uv = p*0.5+0.5; gl_Position = vec4(p,0.,1.); }`;
const FS = `precision highp float;
varying vec2 uv;
uniform sampler2D tex;
uniform float yaw, pitch, hfov, aspect, lensHalf, roll;
uniform mat3 level;           // redressement de l'horizon (identique à geometry.view_matrix)
uniform int raw, nMasks, maskEdit;
uniform vec4 masks[8];
// Échantillonnage avec floutage des zones masquées (compteur, plaque…).
vec4 samp(vec2 q) {
  for (int i = 0; i < 8; i++) {
    if (i >= nMasks) break;
    vec4 m = masks[i];
    if (q.x > m.x && q.x < m.x + m.z && q.y > m.y && q.y < m.y + m.w) {
      vec4 acc = vec4(0.);
      for (int a = -3; a <= 3; a++)
        for (int b = -3; b <= 3; b++)
          acc += texture2D(tex, q + vec2(float(a) * .007, float(b) * .014));
      return acc / 49.;
    }
  }
  return texture2D(tex, q);
}
float edge(vec2 q) {
  float e = 0.;
  for (int i = 0; i < 8; i++) {
    if (i >= nMasks) break;
    vec4 m = masks[i];
    vec2 lo = q - m.xy, hi = m.xy + m.zw - q;
    bool inside = lo.x > -.002 && lo.y > -.004 && hi.x > -.002 && hi.y > -.004;
    if (inside && min(min(lo.x, hi.x), min(lo.y, hi.y) * .5) < .002) e = 1.;
  }
  return e;
}
// Moitié droite du .lrv = objectif avant (+z), moitié gauche = arrière (−z, image en miroir).
vec2 lens(vec3 d, float front) {
  float z = d.z * front, x = d.x * front;
  float rn = acos(clamp(z, -1., 1.)) / lensHalf;
  float h = length(vec2(x, d.y));
  vec2 dir = h > 1e-6 ? vec2(x, d.y) / h : vec2(0.);
  vec2 c = front > 0. ? vec2(.75, .5) : vec2(.25, .5);
  return c + vec2(dir.x * .25, -dir.y * .5) * rn;
}
void main() {
  if (raw == 1) {
    vec2 q = vec2(uv.x, (1. - uv.y - .5) * (aspect / 2.) + .5);
    gl_FragColor = (q.y < 0. || q.y > 1.) ? vec4(0.,0.,0.,1.) : samp(q);
    if (maskEdit == 1 && edge(q) > 0.) gl_FragColor = vec4(.96, .65, .14, 1.);
    return;
  }
  vec2 p = uv * 2. - 1.;
  float t = tan(hfov * .5);
  vec3 d = normalize(vec3(p.x * t, p.y * t / aspect, 1.));
  float cr = cos(roll), sr = sin(roll);   // rotation manuelle de l'image (geometry.view_matrix)
  d = vec3(d.x * cr - d.y * sr, d.x * sr + d.y * cr, d.z);
  float cp = cos(pitch), sp = sin(pitch);
  d = vec3(d.x, d.y * cp + d.z * sp, -d.y * sp + d.z * cp);
  float cy = cos(yaw), sy = sin(yaw);
  d = vec3(d.x * cy + d.z * sy, d.y, -d.x * sy + d.z * cy);
  d = level * d;
  float w = smoothstep(-.04, .04, d.z);
  vec4 a = samp(lens(d, 1.)), b = samp(lens(d, -1.));
  gl_FragColor = mix(b, a, w);
}`;

function initGL() {
  const sh = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src); gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const prog = gl.createProgram();
  gl.attachShader(prog, sh(gl.VERTEX_SHADER, VS));
  gl.attachShader(prog, sh(gl.FRAGMENT_SHADER, FS));
  gl.linkProgram(prog); gl.useProgram(prog);
  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  const loc = gl.getAttribLocation(prog, "p");
  gl.enableVertexAttribArray(loc);
  gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
  gl.bindTexture(gl.TEXTURE_2D, gl.createTexture());
  for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.LINEAR], [gl.TEXTURE_MAG_FILTER, gl.LINEAR],
                        [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]])
    gl.texParameteri(gl.TEXTURE_2D, k, v);
  const u = (n) => gl.getUniformLocation(prog, n);
  return { level: u("level"), roll: u("roll"), yaw: u("yaw"), pitch: u("pitch"), hfov: u("hfov"), aspect: u("aspect"), lensHalf: u("lensHalf"), raw: u("raw"),
           masks: u("masks"), nMasks: u("nMasks"), maskEdit: u("maskEdit") };
}
const U = initGL();

function render() {
  const c = gl.canvas;
  const w = c.clientWidth * devicePixelRatio | 0, h = c.clientHeight * devicePixelRatio | 0;
  if (c.width !== w || c.height !== h) { c.width = w; c.height = h; }
  gl.viewport(0, 0, w, h);
  if (video.readyState >= 2) gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, gl.RGB, gl.UNSIGNED_BYTE, video);
  // Dans un clip à points clés, la vue suit l'interpolation. Un réglage manuel ne tient que
  // sur pause (ou pendant le glisser, ou en mode Auto) : en lecture, on suit toujours les points.
  const follow = !st.viewOverride || (!video.paused && !st.dragging && !st.autoKey);
  if (st.s && follow) {
    const ck = activeClip(now());
    if (ck && clipKeys(ck).length) Object.assign(st.view, clipViewAt(ck, now() - ck.start));
  }
  const rad = Math.PI / 180, v = st.view;
  gl.uniform1f(U.yaw, v.yaw * rad);
  gl.uniform1f(U.pitch, v.pitch * rad);
  gl.uniform1f(U.hfov, v.fov * rad);
  gl.uniform1f(U.aspect, w / h);
  gl.uniform1f(U.lensHalf, LENS_FOV / 2 * rad);
  gl.uniform1i(U.raw, v.raw ? 1 : 0);
  // GLSL attend les matrices par colonnes : on transmet la transposée de la matrice (lignes).
  // Dans un clip sélectionné, on montre exactement son réglage (mode + rotation manuelle).
  const t = st.s ? now() : 0, clip = activeClip(t);
  const L = st.s ? levelMatrix(t, clip ? clipMode(clip) : v.horizon) : [1, 0, 0, 0, 1, 0, 0, 0, 1];
  gl.uniform1f(U.roll, v.roll * rad);
  gl.uniformMatrix3fv(U.level, false, new Float32Array([L[0], L[3], L[6], L[1], L[4], L[7], L[2], L[5], L[8]]));
  const ms = st.maskDraft ? [...st.masks, st.maskDraft] : st.masks;
  const flat = new Float32Array(32);
  ms.slice(0, 8).forEach((m, i) => flat.set([m.x, m.y, m.w, m.h], i * 4));
  gl.uniform4fv(U.masks, flat);
  gl.uniform1i(U.nMasks, Math.min(8, ms.length));
  gl.uniform1i(U.maskEdit, st.maskEdit ? 1 : 0);
  gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
}

// --- Redressement de l'horizon (mêmes formules que geometry.py) ---
function rotRows(axis, deg) {
  const a = deg * Math.PI / 180, c = Math.cos(a), s = Math.sin(a);
  return { p: [1, 0, 0, 0, c, s, 0, -s, c], y: [c, 0, s, 0, 1, 0, -s, 0, c], r: [c, -s, 0, s, c, 0, 0, 0, 1] }[axis];
}
function mul3(a, b) {
  const o = new Array(9).fill(0);
  for (let r = 0; r < 3; r++) for (let c = 0; c < 3; c++) for (let k = 0; k < 3; k++) o[r * 3 + c] += a[r * 3 + k] * b[k * 3 + c];
  return o;
}
function minRotation(u) {   // rotation minimale y → u
  const n = Math.hypot(u[0], u[1], u[2]); const [x, y, z] = [u[0] / n, u[1] / n, u[2] / n];
  const ax = [z, 0, -x], s = Math.hypot(ax[0], ax[2]), c = y;
  if (s < 1e-9) return [1, 0, 0, 0, 1, 0, 0, 0, 1];
  const k = [ax[0] / s, 0, ax[2] / s];
  const K = [0, -k[2], k[1], k[2], 0, -k[0], -k[1], k[0], 0], K2 = mul3(K, K);
  return K.map((v, i) => (i % 4 === 0 ? 1 : 0) + s * v + (1 - c) * K2[i]);
}
function clipMode(c) {   // cf. geometry.clip_horizon_mode
  return ["auto", "fixe", "aucun"].includes(c.horizon) ? c.horizon : (c.level ? "auto" : "aucun");
}
// --- Points clés de cadrage (mêmes formules que geometry.clip_view_at) ---
const EASINGS = {
  linear: (u) => u,
  ease_in_out: (u) => u * u * (3 - 2 * u),
  ease_in: (u) => u * u,
  ease_out: (u) => 1 - (1 - u) ** 2,
  quick: (u) => u ** 3 * (u * (6 * u - 15) + 10),
  delay: (u) => (u < 0.5 ? 0 : ((u - 0.5) * 2) ** 2 * (3 - 4 * (u - 0.5))),
  cut: (u) => (u < 1 ? 0 : 1),
};
const CURVE_LABELS = { linear: "linéaire", ease_in_out: "douce", ease_in: "douce à l'entrée", ease_out: "douce à la sortie",
                       quick: "rapide", delay: "départ retardé", cut: "coupe franche" };
function baseView(c) { return { yaw: c.yaw ?? 0, pitch: c.pitch ?? 0, roll: c.roll ?? 0, fov: c.fov ?? 100 }; }
function clipKeys(c) {
  if (c.keyframes?.length) return [...c.keyframes].sort((a, b) => a.t - b.t);
  if (c.roll_keys?.length) return c.roll_keys.map(([t, r]) => ({ ...baseView(c), t, roll: r, curve: "linear" }));
  return [];
}
function clipViewAt(c, tr) {
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
/** Ajoute ou met à jour le point clé du clip sélectionné à la tête de lecture, depuis la vue affichée. */
function upsertKey(c, t) {
  if (c.roll_keys) { c.keyframes = clipKeys(c); delete c.roll_keys; }
  c.keyframes = c.keyframes || [];
  const tr = +Math.max(0, Math.min(c.end - c.start, t - c.start)).toFixed(2);
  const v = { yaw: +st.view.yaw.toFixed(1), pitch: +st.view.pitch.toFixed(1), roll: +st.view.roll.toFixed(1), fov: +st.view.fov.toFixed(1) };
  const near = c.keyframes.find((k) => Math.abs(k.t - tr) < 0.15);
  const same = (k) => k && ["yaw", "pitch", "roll", "fov"].every((f) => Math.abs(k[f] - v[f]) < 0.05);
  const sorted = [...c.keyframes].sort((a, b) => a.t - b.t);
  const before = sorted.filter((k) => k.t < tr).pop(), after = sorted.find((k) => k.t > tr);
  if (near) Object.assign(near, v);
  else if (same(before) && (!after || same(after))) { st.viewOverride = false; return; }   // rien de nouveau
  else {
    // un point inséré hérite de la courbe du segment ; ajouté à la fin, il est linéaire (comme l'app)
    const prev = [...c.keyframes].sort((a, b) => a.t - b.t).filter((k) => k.t < tr).pop();
    const isLast = !c.keyframes.some((k) => k.t > tr);
    c.keyframes.push({ t: tr, ...v, curve: prev && !isLast ? prev.curve : "linear" });
  }
  c.keyframes.sort((a, b) => a.t - b.t);
  st.viewOverride = false;
  saveClips(c);
}
/** Clip sous la tête de lecture : le sélectionné en priorité, sinon n'importe quel clip traversé. */
function activeClip(t) {
  const inside = (c) => c && t >= c.start - 0.05 && t <= c.end + 0.05;
  const c = st.clips[st.sel];
  return inside(c) ? c : st.clips.find(inside) || null;
}
function levelMatrix(t, mode) {
  if (mode === "aucun") return [1, 0, 0, 0, 1, 0, 0, 0, 1];
  const h = st.horizon;
  if (mode === "auto" && h && h.up) {
    const x = Math.max(0, Math.min(h.up.length - 1.001, t * h.hz)), i = Math.floor(x), f = x - i;
    const a = h.up[i], b = h.up[i + 1];
    return minRotation([a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f]);
  }
  const tl = st.s.tilt || { pitch: 0, roll: 0 };
  return mul3(rotRows("p", tl.pitch), rotRows("r", tl.roll));
}

let horizonTimer = null;
async function loadHorizon(id) {
  clearTimeout(horizonTimer);
  const h = await api("GET", `/api/horizon/${id}`);
  if (!st.s || st.s.id !== id) return;
  st.horizon = h.status === "done" ? h : null;
  st.horizonStatus = h.status === "done" ? "horizon : image"
    : h.status === "error" ? "horizon : fixe (erreur d'analyse)"
    : h.status === "queued" ? "horizon : fixe · analyse en attente"
    : `horizon : fixe · analyse ${Math.round((h.progress || 0) * 100)} %`;
  if (h.status !== "done" && h.status !== "error") horizonTimer = setTimeout(() => loadHorizon(id), 2000);
}

function setView(p) {
  Object.assign(st.view, p);
  st.view.yaw = ((st.view.yaw + 540) % 360) - 180;
  st.view.pitch = Math.max(-89, Math.min(89, st.view.pitch));
  st.view.fov = Math.max(30, Math.min(150, st.view.fov));
  $("#view-raw").classList.toggle("active", st.view.raw);
  st.view.roll = Math.max(-45, Math.min(45, st.view.roll ?? 0));
  $("#horizon-mode").value = st.view.horizon;
  $("#view-roll").value = st.view.roll;
  $("#viewinfo").textContent = st.view.raw ? "brut"
    : `yaw ${st.view.yaw.toFixed(0)}° · pitch ${st.view.pitch.toFixed(0)}° · rot. ${st.view.roll.toFixed(1)}° · champ ${st.view.fov.toFixed(0)}°`;
}

/** Changement de vue par l'utilisateur : suspend le suivi des points clés, ou enregistre (Auto). */
let autoKeyTimer = null;
function userView(p) {
  setView(p);
  const c = activeClip(now());
  if (!c) return;
  st.viewOverride = true;
  // Comme l'app Insta360 : sur un point clé (ou en mode Auto), le changement le met à jour directement.
  const onKey = clipKeys(c).some((k) => Math.abs(k.t - (now() - c.start)) < 0.1);
  if (st.autoKey || onKey) { clearTimeout(autoKeyTimer); autoKeyTimer = setTimeout(() => upsertKey(c, now()), 200); }
}

{
  let drag = null;
  const cv = $("#gl");
  // Pixel du canvas → coordonnées dans l'image .lrv (vue brute, letterbox 2:1).
  const texCoord = (e) => {
    const r = cv.getBoundingClientRect();
    const px = (e.clientX - r.left) / r.width, py = (e.clientY - r.top) / r.height;
    return { x: Math.max(0, Math.min(1, px)), y: Math.max(0, Math.min(1, (py - .5) * (r.width / r.height / 2) + .5)) };
  };
  const angleAt = (e) => {   // angle du pointeur autour du centre de la vue (°)
    const r = cv.getBoundingClientRect();
    return Math.atan2(e.clientY - (r.top + r.height / 2), e.clientX - (r.left + r.width / 2)) * 180 / Math.PI;
  };
  cv.addEventListener("pointerdown", (e) => {
    drag = { x: e.clientX, y: e.clientY, q: texCoord(e), rotate: e.ctrlKey || e.metaKey, a: angleAt(e) };
    st.dragging = true;
    cv.classList.toggle("rotating", drag.rotate);
    cv.setPointerCapture(e.pointerId);
  });
  cv.addEventListener("pointerup", () => {
    cv.classList.remove("rotating");
    st.dragging = false;
    const m = st.maskDraft;
    st.maskDraft = null; drag = null;
    if (m && m.w > .005 && m.h > .01) { st.masks.push(m); saveSettings(); }
  });
  cv.addEventListener("pointermove", (e) => {
    if (drag && st.maskEdit) {
      const q = texCoord(e);
      st.maskDraft = { x: Math.min(q.x, drag.q.x), y: Math.min(q.y, drag.q.y),
                       w: Math.abs(q.x - drag.q.x), h: Math.abs(q.y - drag.q.y) };
      return;
    }
    if (!drag || st.view.raw) return;
    if (drag.rotate) {   // Ctrl + glisser : on « attrape » l'image et on la fait tourner
      const a = angleAt(e);
      const d = ((a - drag.a + 540) % 360) - 180;
      drag.a = a;
      userView({ roll: st.view.roll + d });
      return;
    }
    const k = st.view.fov / cv.clientWidth;
    userView({ yaw: st.view.yaw - (e.clientX - drag.x) * k, pitch: st.view.pitch + (e.clientY - drag.y) * k });
    drag = { x: e.clientX, y: e.clientY };
  });
  cv.addEventListener("wheel", (e) => { e.preventDefault(); userView({ fov: st.view.fov * (e.deltaY > 0 ? 1.08 : 1 / 1.08) }); },
                      { passive: false });
}

// Vue 16:9 ajustée à l'espace disponible (letterbox) ; la carte suit les redimensionnements.
new ResizeObserver(() => {
  const stage = $(".stage"), W = stage.clientWidth, H = stage.clientHeight;
  let w = W, h = W * 9 / 16;
  if (h > H) { h = H; w = H * 16 / 9; }
  gl.canvas.style.width = Math.floor(w) + "px";
  gl.canvas.style.height = Math.floor(h) + "px";
}).observe($(".stage"));

async function loadSettings() {
  const cfg = await api("GET", "/api/settings");
  st.masks = cfg.masks || [];
  st.telemetry = cfg.telemetry || {};
  document.querySelectorAll("[data-tel]").forEach((cb) => (cb.checked = !!st.telemetry[cb.dataset.tel]));
  updateMaskUI();
}
document.querySelectorAll("[data-tel]").forEach((cb) => cb.addEventListener("change", () => {
  st.telemetry = { ...st.telemetry, [cb.dataset.tel]: cb.checked };
  api("PUT", "/api/settings", { telemetry: st.telemetry }).catch(console.error);
}));
function saveSettings() {
  updateMaskUI();
  api("PUT", "/api/settings", { masks: st.masks, telemetry: st.telemetry }).catch(console.error);
}
function updateMaskUI() {
  $("#mask-edit").classList.toggle("active", st.maskEdit);
  $("#mask-edit").textContent = st.maskEdit ? "Terminer masque" : "Masquer…";
  $("#mask-clear").disabled = !st.masks.length;
  $("#mask-clear").textContent = `✕ masques (${st.masks.length})`;
  $("#gl").classList.toggle("masking", st.maskEdit);
}
$("#mask-edit").addEventListener("click", () => {
  st.maskEdit = !st.maskEdit;
  if (st.maskEdit) { setView({ raw: true }); video.pause(); } else setView({ raw: false });
  updateMaskUI();
});
$("#mask-clear").addEventListener("click", () => { st.masks = []; saveSettings(); });

// ------------------------------------------------------------------ timeline

const tl = $("#timeline");
const tctx = tl.getContext("2d");
const TRACKS = [   // [clé, libellé, hauteur, couleur, type]
  ["score", "intérêt", 22, null, "heat"],
  ["speed", "vitesse", 38, "--speed", "line"],
  ["turn", "virages", 30, "--turn", "bars"],
  ["alt", "altitude", 28, "--alt", "area"],
  ["vib", "vibrations", 22, "--muted", "line"],
];
const css = (v) => getComputedStyle(document.documentElement).getPropertyValue(v).trim();
const LABEL_W = 70;
const xOf = (t) => LABEL_W + (t - st.tl.v0) / (st.tl.v1 - st.tl.v0) * (tl.clientWidth - LABEL_W);
const tOf = (x) => st.tl.v0 + (x - LABEL_W) / (tl.clientWidth - LABEL_W) * (st.tl.v1 - st.tl.v0);

function followTimeline(t) {
  const span = st.tl.v1 - st.tl.v0;
  if (span >= st.s.duration) return;
  if (t < st.tl.v0 || t > st.tl.v1) {
    st.tl.v0 = Math.max(0, Math.min(st.s.duration - span, t - span * 0.2));
    st.tl.v1 = st.tl.v0 + span;
  }
}

function heat(v) {  // 0 → sombre, 1 → ambre vif
  const a = Math.max(0, Math.min(1, v));
  return `rgba(245,165,36,${(a * a).toFixed(3)})`;
}

function drawTimeline() {
  const w = tl.clientWidth, h = tl.clientHeight, dpr = devicePixelRatio;
  if (tl.width !== w * dpr || tl.height !== h * dpr) { tl.width = w * dpr; tl.height = h * dpr; }
  tctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  tctx.clearRect(0, 0, w, h);
  if (!st.s) return;
  const S = st.s.series, n = st.s.duration;
  const i0 = Math.max(0, Math.floor(st.tl.v0)), i1 = Math.min(n, Math.ceil(st.tl.v1));
  const step = Math.max(1, Math.floor((i1 - i0) / (w - LABEL_W)));
  let y = 16 + KEY_LANE;                                           // ligne des points clés en haut
  const k = (h - 18 - KEY_LANE) / TRACKS.reduce((s, tr) => s + tr[2], 0);  // pistes à l'échelle du canvas
  tctx.font = "12px system-ui"; tctx.textBaseline = "middle";

  // graduations horaires
  const span = st.tl.v1 - st.tl.v0;
  const tick = [10, 30, 60, 300, 600, 900, 1800, 3600].find((s) => span / s < 12) || 3600;
  tctx.fillStyle = css("--muted"); tctx.strokeStyle = css("--line");
  for (let t = Math.ceil(st.tl.v0 / tick) * tick; t <= st.tl.v1; t += tick) {
    const x = xOf(t);
    tctx.beginPath(); tctx.moveTo(x, 12); tctx.lineTo(x, h); tctx.stroke();
    tctx.fillText(localClock(t).slice(0, tick < 60 ? 8 : 5), x + 3, 6);
  }

  for (const [key, label, th0, color, type] of TRACKS) {
    const th = th0 * k;
    const arr = S[key];
    tctx.fillStyle = css("--muted"); tctx.fillText(label, 6, y + th / 2);
    const vals = arr.filter((v) => v !== null);
    if (vals.length) {
      let lo = Math.min(...vals), hi = Math.max(...vals);
      if (key === "turn") hi = Math.min(hi, 15);
      if (key === "score") { lo = 0; hi = 1; }
      const Y = (v) => y + th - 2 - (Math.min(v, hi) - lo) / (hi - lo || 1) * (th - 4);
      tctx.strokeStyle = tctx.fillStyle = color ? css(color) : "#fff";
      if (type === "line" || type === "area") tctx.beginPath();
      let started = false;
      for (let i = i0; i < i1; i += step) {
        let v = arr[i];
        if (v === null) { started = false; continue; }
        if (step > 1 && type !== "line") for (let j = 1; j < step && i + j < i1; j++) v = Math.max(v, arr[i + j] ?? v);
        const x = xOf(i), x2 = xOf(i + step);
        if (type === "heat") { tctx.fillStyle = heat(v); tctx.fillRect(x, y, x2 - x + 0.5, th - 2); }
        else if (type === "bars") tctx.fillRect(x, Y(v), Math.max(1, x2 - x), y + th - 2 - Y(v));
        else if (!started) { tctx.moveTo(x, Y(v)); started = true; }
        else tctx.lineTo(x, Y(v));
      }
      if (type === "line") tctx.stroke();
      if (type === "area") { tctx.lineWidth = 1.5; tctx.stroke(); tctx.lineWidth = 1; }
    }
    y += th;
  }
  const bottom = y;

  // clips (le clip sélectionné a des poignées déplaçables)
  st.clips.forEach((c, k) => {
    const x0 = xOf(c.start), x1 = xOf(c.end), sel = k === st.sel;
    tctx.fillStyle = sel ? "rgba(62,207,142,.30)" : "rgba(62,207,142,.15)";
    tctx.fillRect(x0, 12, x1 - x0, bottom - 12);
    tctx.fillStyle = css("--clip"); tctx.fillRect(x0, 12, x1 - x0, 3);
    if (x1 - x0 > 14) { tctx.font = "bold 12px system-ui"; tctx.fillText(k + 1, x0 + 4, 22); tctx.font = "12px system-ui"; }
    if (sel) for (const x of [x0, x1]) {
      tctx.fillRect(x - 2, 12, 4, bottom - 12);
      tctx.fillRect(x - 5, bottom / 2 - 10, 10, 20);
    }
  });
  if (st.inPoint !== null) {  // clip en cours de création : du début posé à la tête de lecture
    const a = xOf(Math.min(st.inPoint, now())), b = xOf(Math.max(st.inPoint, now()));
    tctx.fillStyle = "rgba(62,207,142,.12)"; tctx.fillRect(a, 12, b - a, bottom - 12);
    tctx.fillStyle = css("--clip"); tctx.fillRect(xOf(st.inPoint) - 1, 12, 2, bottom - 12);
  }
  // candidats
  tctx.fillStyle = css("--accent");
  st.s.candidates.forEach((t) => {
    const x = xOf(t);
    tctx.beginPath(); tctx.moveTo(x - 5, 12); tctx.lineTo(x + 5, 12); tctx.lineTo(x, 19); tctx.fill();
  });
  // segments
  tctx.strokeStyle = "#555"; tctx.setLineDash([3, 3]);
  st.s.segments.slice(1).forEach((sg) => { const x = xOf(sg.offset); tctx.beginPath(); tctx.moveTo(x, 12); tctx.lineTo(x, bottom); tctx.stroke(); });
  tctx.setLineDash([]);
  drawKeyLane(bottom);
  // curseur survolé + tête de lecture
  if (st.hover !== null) {
    const t = st.hover, x = xOf(t);
    tctx.strokeStyle = "rgba(255,255,255,.35)"; tctx.beginPath(); tctx.moveTo(x, 12); tctx.lineTo(x, bottom); tctx.stroke();
    const sp = series("speed", t), al = series("alt", t);
    const txt = `${localClock(t)}  ${sp !== null ? sp.toFixed(0) + " km/h" : "—"}  ${al !== null ? al + " m" : ""}`;
    tctx.fillStyle = "rgba(0,0,0,.7)";
    const tw = tctx.measureText(txt).width + 10, tx = Math.min(x + 6, w - tw);
    tctx.fillRect(tx, bottom - 18, tw, 16); tctx.fillStyle = "#fff"; tctx.fillText(txt, tx + 5, bottom - 10);
  }
  const px = xOf(now());
  tctx.fillStyle = css("--play"); tctx.fillRect(px - 1, 12, 2, bottom - 12);
}

const KEY_LANE = 18;         // hauteur de la ligne « ◆ points » en haut de la timeline
const KEY_Y = 16 + KEY_LANE / 2;
/** Ligne dédiée aux points clés : losanges, liaisons (pointillés = coupe franche), repères verticaux. */
function drawKeyLane(bottom) {
  const w = tl.clientWidth;
  tctx.fillStyle = "rgba(245,165,36,.08)";
  tctx.fillRect(LABEL_W, 16, w - LABEL_W, KEY_LANE);
  tctx.fillStyle = css("--accent");
  tctx.fillText("◆ points", 6, KEY_Y);
  st.clips.forEach((c, ci) => {
    const keys = clipKeys(c);
    if (!keys.length) return;
    const sel = ci === st.sel;
    for (let i = 0; i + 1 < keys.length; i++) {   // liaison vers le point suivant
      const x0 = xOf(c.start + keys[i].t), x1 = xOf(c.start + keys[i + 1].t);
      tctx.strokeStyle = css("--accent"); tctx.lineWidth = sel ? 2 : 1;
      tctx.setLineDash(keys[i].curve === "cut" ? [3, 3] : []);
      tctx.beginPath(); tctx.moveTo(x0, KEY_Y); tctx.lineTo(x1, KEY_Y); tctx.stroke();
    }
    tctx.setLineDash([]); tctx.lineWidth = 1;
    for (const kf of keys) {
      const x = xOf(c.start + kf.t), here = sel && Math.abs(now() - c.start - kf.t) < 0.05;
      tctx.strokeStyle = "rgba(245,165,36,.35)";                     // repère à travers les pistes
      tctx.beginPath(); tctx.moveTo(x, 16 + KEY_LANE); tctx.lineTo(x, bottom); tctx.stroke();
      const r = sel ? 7 : 5;
      tctx.fillStyle = here ? "#fff" : css("--accent");
      tctx.beginPath(); tctx.moveTo(x, KEY_Y - r); tctx.lineTo(x + r, KEY_Y); tctx.lineTo(x, KEY_Y + r); tctx.lineTo(x - r, KEY_Y); tctx.fill();
    }
  });
}
function keyAt(x, y) {
  if (Math.abs(y - KEY_Y) > 8) return null;
  for (let k = 0; k < st.clips.length; k++) {
    const keys = clipKeys(st.clips[k]);
    for (let i = 0; i < keys.length; i++) if (Math.abs(xOf(st.clips[k].start + keys[i].t) - x) <= 6) return { k, key: keys[i] };
  }
  return null;
}
// Bord de clip sous la souris (±6 px) : on privilégie le clip sélectionné.
function edgeAt(x) {
  const order = st.clips.map((c, k) => k).sort((a, b) => (b === st.sel) - (a === st.sel));
  for (const k of order) for (const edge of ["start", "end"])
    if (Math.abs(xOf(st.clips[k][edge]) - x) <= 6) return { k, edge };
  return null;
}
let tlDrag = null;
tl.addEventListener("mousedown", (e) => {
  if (!st.s || e.offsetX <= LABEL_W) return;
  const kh = keyAt(e.offsetX, e.offsetY);
  if (kh) {
    const c = st.clips[kh.k];
    if (c.roll_keys) { c.keyframes = clipKeys(c); delete c.roll_keys; }
    tlDrag = { k: kh.k, key: c.keyframes.find((x) => x.t === kh.key.t), moved: false };
    st.sel = kh.k; renderClips(); e.preventDefault();
    return;
  }
  const hit = edgeAt(e.offsetX);
  if (hit) { tlDrag = { ...hit, moved: false }; st.sel = hit.k; renderEditor(); e.preventDefault(); }
});
window.addEventListener("mousemove", (e) => {
  if (!tlDrag) return;
  const r = tl.getBoundingClientRect();
  const c = st.clips[tlDrag.k], t = tOf(e.clientX - r.left);
  if (tlDrag.key) {   // déplacement d'un point clé dans le clip
    tlDrag.key.t = +Math.max(0, Math.min(c.end - c.start, t - c.start)).toFixed(2);
    c.keyframes.sort((a, b) => a.t - b.t);
    tlDrag.moved = true;
    st.viewOverride = false;
    seek(c.start + tlDrag.key.t);
    renderEditor();
    return;
  }
  if (tlDrag.edge === "start") c.start = +Math.max(0, Math.min(t, c.end - 0.5)).toFixed(2);
  else c.end = +Math.min(st.s.duration, Math.max(t, c.start + 0.5)).toFixed(2);
  tlDrag.moved = true;
  seek(c[tlDrag.edge]);                 // on voit l'image du bord pendant le réglage
  renderEditor();
});
window.addEventListener("mouseup", () => {
  if (!tlDrag) return;
  const c = st.clips[tlDrag.k];
  if (tlDrag.key && !tlDrag.moved) { st.viewOverride = false; seek(c.start + tlDrag.key.t); }
  if (tlDrag.moved) saveClips(c);
  setTimeout(() => (tlDrag = null), 0);  // évite le clic « seek » qui suit le relâchement
});
tl.addEventListener("mousemove", (e) => {
  st.hover = e.offsetX > LABEL_W ? tOf(e.offsetX) : null;
  tl.style.cursor = tlDrag || (st.s && (edgeAt(e.offsetX) || keyAt(e.offsetX, e.offsetY))) ? "ew-resize" : "crosshair";
});
tl.addEventListener("mouseleave", () => (st.hover = null));
tl.addEventListener("click", (e) => {
  if (!st.s || e.offsetX <= LABEL_W || tlDrag) return;
  const t = tOf(e.offsetX);
  const k = st.clips.findIndex((c) => t >= c.start && t <= c.end);
  if (k >= 0 && k !== st.sel) { st.sel = k; renderClips(); }
  seek(t);
});
tl.addEventListener("wheel", (e) => {
  if (!st.s) return;
  e.preventDefault();
  const t = tOf(Math.max(LABEL_W, e.offsetX));
  const k = e.deltaY > 0 ? 1.25 : 0.8;
  let span = Math.max(30, Math.min(st.s.duration, (st.tl.v1 - st.tl.v0) * k));
  let v0 = t - (t - st.tl.v0) * span / (st.tl.v1 - st.tl.v0);
  v0 = Math.max(0, Math.min(st.s.duration - span, v0));
  st.tl = { v0, v1: v0 + span };
}, { passive: false });

// ------------------------------------------------------------------ carte

const map = L.map("map", { zoomControl: true, attributionControl: true });
new ResizeObserver(() => map.invalidateSize()).observe($("#map"));
L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
  maxZoom: 18, attribution: "© OpenStreetMap",
}).addTo(map);
map.setView([45.76, 4.84], 9);
let mapLayers = L.layerGroup().addTo(map), clipLayer = L.layerGroup().addTo(map);
const posMarker = L.circleMarker([0, 0], { radius: 7, color: "#fff", weight: 2, fillColor: "#ff4d4f", fillOpacity: 1 });

function drawMap() {
  mapLayers.clearLayers();
  const { lat, lon, score } = st.s.series;
  const pts = [];
  for (let i = 0; i < lat.length; i += 5) {
    if (lat[i] === null) continue;
    pts.push([lat[i], lon[i], i]);
  }
  if (!pts.length) { posMarker.remove(); return; }
  for (let k = 1; k < pts.length; k++) {
    const [a, b] = [pts[k - 1], pts[k]];
    if (b[2] - a[2] > 30) continue;
    L.polyline([[a[0], a[1]], [b[0], b[1]]], { color: heatColor(score[b[2]]), weight: 4, opacity: 0.9 }).addTo(mapLayers);
  }
  st.s.candidates.forEach((t, k) => {
    if (lat[t] === null) return;
    L.circleMarker([lat[t], lon[t]], { radius: 5, color: "#f5a524", fillOpacity: 1 })
      .bindTooltip(`candidat ${k + 1} · ${localClock(t)}`).on("click", () => seek(t)).addTo(mapLayers);
  });
  posMarker.addTo(map);
  map.fitBounds(L.latLngBounds(pts.map((p) => [p[0], p[1]])), { padding: [20, 20] });
  map.off("click").on("click", (e) => {
    let best = null, bd = Infinity;
    for (const [la, lo, i] of pts) {
      const d = (la - e.latlng.lat) ** 2 + (lo - e.latlng.lng) ** 2;
      if (d < bd) { bd = d; best = i; }
    }
    if (best !== null) seek(best);
  });
}

function heatColor(v) {
  // bleu (calme) → ambre (intéressant)
  const a = Math.max(0, Math.min(1, v ?? 0));
  const r = Math.round(90 + a * (245 - 90)), g = Math.round(169 + a * (165 - 169)), b = Math.round(255 + a * (36 - 255));
  return `rgb(${r},${g},${b})`;
}

function drawClipsOnMap() {
  clipLayer.clearLayers();
  const { lat, lon } = st.s.series;
  for (const c of st.clips) {
    const line = [];
    for (let i = Math.floor(c.start); i <= Math.ceil(c.end) && i < lat.length; i++) if (lat[i] !== null) line.push([lat[i], lon[i]]);
    if (line.length > 1) L.polyline(line, { color: "#3ecf8e", weight: 8, opacity: 0.7 }).addTo(clipLayer);
  }
}

function updateMapPos(t) {
  const { lat, lon } = st.s.series;
  const i = Math.floor(t);
  if (lat[i] != null) posMarker.setLatLng([lat[i], lon[i]]);
}

// ------------------------------------------------------------------ clips

const QUICK_CLIP = [5, 10];        // « + Clip » : 5 s avant, 10 s après la tête de lecture
const fmtPrecise = (t) => localClock(t) + "." + Math.floor((t % 1) * 10);

let saveTimer = null;
function saveClips(keep) {
  const sel = st.clips[st.sel];
  st.clips.sort((a, b) => a.start - b.start);
  st.sel = sel ? st.clips.indexOf(keep ?? sel) : null;
  renderClips(); drawClipsOnMap();
  clearTimeout(saveTimer);
  const sid = st.s.id, clips = st.clips.map((c) => ({ ...c }));
  saveTimer = setTimeout(() => api("PUT", `/api/selections/${sid}`, clips).catch(console.error), 300);
}

function currentViewParams() {
  const v = st.view;
  const view = v.raw ? { yaw: 0, pitch: -10, fov: 100, roll: 0 }
    : { yaw: +v.yaw.toFixed(1), pitch: +v.pitch.toFixed(1), fov: +v.fov.toFixed(1), roll: +v.roll.toFixed(1) };
  return { ...view, horizon: v.horizon };
}

function addClip(start, end) {
  start = Math.max(0, start); end = Math.min(st.s.duration, end);
  if (end - start < 0.5) return;
  const c = { start: +start.toFixed(2), end: +end.toFixed(2), ...currentViewParams() };
  st.clips.push(c);
  st.sel = st.clips.length - 1;
  saveClips(c);
}

function markIn() { st.inPoint = now(); updateMarkUI(); }
function markOut() {
  const t = now();
  if (st.inPoint === null) { $("#mark-hint").textContent = "Pose d'abord un début (I)"; return; }
  const [a, b] = [Math.min(st.inPoint, t), Math.max(st.inPoint, t)];
  st.inPoint = null;
  addClip(a, b);
  updateMarkUI();
}
function quickClip() { const t = now(); addClip(t - QUICK_CLIP[0], t + QUICK_CLIP[1]); }

function updateMarkUI() {
  $("#mark-in").classList.toggle("active", st.inPoint !== null);
  $("#mark-hint").textContent = st.inPoint !== null ? `début ${fmtPrecise(st.inPoint)} → O pour la fin` : "";
}

function selectClip(k, go) {
  st.sel = k;
  const c = st.clips[k];
  if (c && go) {
    setView({ yaw: c.yaw, pitch: c.pitch, fov: c.fov, horizon: clipMode(c), raw: false });
    seek(c.start, go === "play" ? true : undefined);
  }
  renderClips();
}

function editEdge(edge, value) {
  const c = st.clips[st.sel];
  if (!c) return;
  if (edge === "start") c.start = +Math.max(0, Math.min(value, c.end - 0.5)).toFixed(2);
  else c.end = +Math.min(st.s.duration, Math.max(value, c.start + 0.5)).toFixed(2);
  saveClips(c);
}

// Suppression en masse : premier clic arme le bouton, second clic (dans les 3 s) confirme.
let bulkArmed = null;
function updateBulk() {
  for (const c of st.checked) if (!st.clips.includes(c)) st.checked.delete(c);
  const n = st.checked.size, b = $("#delete-checked");
  b.disabled = n === 0;
  b.textContent = bulkArmed ? `Confirmer (${n}) ?` : n ? `Supprimer ${n}` : "Supprimer";
  const all = $("#check-all");
  all.checked = n > 0 && n === st.clips.length;
  all.indeterminate = n > 0 && n < st.clips.length;
}
$("#check-all").addEventListener("change", (e) => {
  st.checked = e.target.checked ? new Set(st.clips) : new Set();
  renderClips();
});
$("#delete-checked").addEventListener("click", () => {
  if (!bulkArmed) {
    bulkArmed = setTimeout(() => { bulkArmed = null; updateBulk(); }, 3000);
    updateBulk();
    return;
  }
  clearTimeout(bulkArmed); bulkArmed = null;
  const sel = st.clips[st.sel];
  st.clips = st.clips.filter((c) => !st.checked.has(c));
  st.checked.clear();
  st.sel = sel && st.clips.includes(sel) ? st.clips.indexOf(sel) : null;
  saveClips();
});

function deleteSelected() {
  if (st.sel === null || !st.clips[st.sel]) return;
  st.clips.splice(st.sel, 1);
  st.sel = null;
  saveClips();
}

function renderClips() {
  const ol = $("#clips");
  ol.innerHTML = "";
  const t = now();
  st.clips.forEach((c, k) => {
    const li = document.createElement("li");
    li.classList.toggle("current", t >= c.start && t <= c.end);
    li.classList.toggle("selected", k === st.sel);
    li.innerHTML = `<input type="checkbox" class="chk" ${st.checked.has(c) ? "checked" : ""}>
      <span class="n">${k + 1}</span>
      <div><div>${localClock(c.start)} → ${localClock(c.end)} <span class="muted">(${fmt(c.end - c.start)})</span></div>
      <div class="meta">yaw ${c.yaw}° · pitch ${c.pitch}° · champ ${c.fov}° · horizon ${clipMode(c)}${clipKeys(c).length ? ` · ◆ ${clipKeys(c).length}` : ""}</div></div>
      <div class="act"><button data-a="play" title="Lire le clip">▶</button></div>`;
    li.addEventListener("click", (e) => {
      if (e.target.classList.contains("chk")) {
        e.target.checked ? st.checked.add(c) : st.checked.delete(c);
        updateBulk();
        return;
      }
      selectClip(k, e.target.dataset.a === "play" ? "play" : "seek");
    });
    ol.appendChild(li);
  });
  const total = st.clips.reduce((s, c) => s + c.end - c.start, 0);
  $("#clips-total").textContent = st.clips.length ? `${st.clips.length} · ${fmt(total)}` : "aucun";
  if (!st.clips.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.innerHTML = `Aucun clip pour l'instant.<br>Pendant la lecture : <kbd>I</kbd> puis <kbd>O</kbd> pour poser début et fin,
      ou <kbd>C</kbd> pour un clip de 15 s. <kbd>N</kbd> saute au prochain moment fort (ambre sur la frise).`;
    ol.appendChild(li);
  }
  renderEditor();
  updateBulk();
  renderStats();
}

function renderEditor() {
  const c = st.clips[st.sel], ed = $("#clip-editor");
  ed.hidden = !c;
  if (!c) return;
  $("#ed-title").textContent = `Clip ${st.sel + 1} · ${fmt(c.end - c.start)}`;
  $("#ed-start").textContent = fmtPrecise(c.start);
  $("#ed-end").textContent = fmtPrecise(c.end);
  $("#ed-view").textContent = `yaw ${c.yaw}° · pitch ${c.pitch}° · champ ${c.fov}°`;
  $("#ed-horizon").value = clipMode(c);
  $("#auto-key").checked = st.autoKey;
  renderKeyList(c);
  $("#ed-loop").classList.toggle("active", st.loop);
}

$("#clip-editor").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  const c = st.clips[st.sel];
  if (!b || !c) return;
  const { edge, d, a } = b.dataset;
  if (edge && d === "here") editEdge(edge, now());
  else if (edge) editEdge(edge, c[edge] + +d);
  else if (a === "view") { Object.assign(c, currentViewParams(), { horizon: clipMode(c) }); saveClips(c); }
  else if (a === "clear-keys") { delete c.roll_keys; delete c.keyframes; saveClips(c); }
  else if (a === "add-key") upsertKey(c, now());
  else if (a === "play") selectClip(st.sel, "play");
  else if (a === "goto-end") seek(Math.max(c.start, c.end - 3), true);
  else if (a === "loop") { st.loop = !st.loop; renderEditor(); }
  else if (a === "del") deleteSelected();
  else if (a === "close") { st.sel = null; renderClips(); }
});
$("#ed-horizon").addEventListener("change", (e) => {
  const c = st.clips[st.sel];
  if (!c) return;
  c.horizon = e.target.value;
  delete c.level;
  setView({ horizon: c.horizon });
  saveClips(c);
});

function renderKeyList(c) {
  const ol = $("#ed-keylist"), keys = clipKeys(c);
  ol.innerHTML = keys.length ? "" : `<li class="muted">Aucun point : cadrage fixe du clip. Place la lecture, cadre, puis ◆.</li>`;
  keys.forEach((k, i) => {
    const li = document.createElement("li");
    li.classList.toggle("here", Math.abs(now() - c.start - k.t) < 0.05);
    const last = i === keys.length - 1;
    li.innerHTML = `<button data-k="go" title="Aller à ce point">◆ ${fmt(c.start + k.t)}.${Math.floor((k.t % 1) * 10)}</button>
      <span class="muted">${k.yaw.toFixed(0)}° / ${k.pitch.toFixed(0)}° / rot ${k.roll.toFixed(0)}° / ${k.fov.toFixed(0)}°</span>
      <span>${last ? "" : `<select data-k="curve" title="Transition vers le point suivant">${Object.entries(CURVE_LABELS)
        .map(([v, l]) => `<option value="${v}" ${v === (k.curve || "linear") ? "selected" : ""}>${l}</option>`).join("")}</select>`}
      <button data-k="del" title="Supprimer ce point">✕</button></span>`;
    li.addEventListener("click", (e) => {
      const what = e.target.dataset.k;
      if (what === "go") { st.viewOverride = false; seek(c.start + k.t); }
      else if (what === "del") { c.keyframes = keys.filter((x) => x !== k); delete c.roll_keys; saveClips(c); }
    });
    li.querySelector("select")?.addEventListener("change", (e) => {
      c.keyframes = keys; delete c.roll_keys;
      k.curve = e.target.value; saveClips(c);
    });
    ol.appendChild(li);
  });
}
$("#auto-key").addEventListener("change", (e) => (st.autoKey = e.target.checked));
$("#view-roll").addEventListener("input", (e) => userView({ roll: +e.target.value }));
$("#mark-in").addEventListener("click", markIn);
$("#mark-out").addEventListener("click", markOut);
$("#quick-clip").addEventListener("click", quickClip);

// ------------------------------------------------------------------ statistiques

document.querySelectorAll(".tabs button").forEach((b) => b.addEventListener("click", () => {
  document.querySelectorAll(".tabs button").forEach((x) => x.classList.toggle("active", x === b));
  $("#map").hidden = b.dataset.tab !== "map";
  $("#stats").hidden = b.dataset.tab !== "stats";
  if (b.dataset.tab === "map") map.invalidateSize();
}));

function renderStats() {
  const s = st.s?.stats, el = $("#stats");
  if (!s) return;
  const tile = (v, l) => `<div class="stat"><div class="v">${v}</div><div class="l">${l}</div></div>`;
  const clipTime = st.clips.reduce((a, c) => a + c.end - c.start, 0);
  let clipKm = 0;
  const { lat, lon } = st.s.series;
  for (const c of st.clips) for (let i = Math.ceil(c.start); i < Math.floor(c.end); i++) {
    if (lat[i] == null || lat[i + 1] == null) continue;
    const dy = (lat[i + 1] - lat[i]) * 111.2, dx = (lon[i + 1] - lon[i]) * 111.2 * Math.cos(lat[i] * Math.PI / 180);
    clipKm += Math.hypot(dx, dy);
  }
  let html = `<div class="stat-section">Session</div><div class="stat-grid">
    ${tile(fmt(s.duration_s), "durée filmée")}`;
  if (s.distance_km !== undefined) {
    html += `${tile(s.distance_km + " km", "distance")}
      ${tile(s.avg_speed_kmh + " km/h", "moyenne en roulant")}
      ${tile(s.max_speed_kmh + " km/h", "vitesse max")}
      ${tile(`${s.alt_min_m}–${s.alt_max_m} m`, "altitude")}
      ${tile(`+${s.climb_m} / −${s.descent_m} m`, "dénivelé (≈, GPS)")}
      ${tile(`${st.s.tilt.pitch.toFixed(0)}° / ${st.s.tilt.roll.toFixed(0)}°`, "inclinaison caméra (pitch / roll)")}</div>`;
  } else html += `</div><p class="muted">Pas de GPS pour cette session : stats de trajet indisponibles.</p>`;
  html += `<div class="stat-section">Montage</div><div class="stat-grid">
    ${tile(st.clips.length, "clips")}
    ${tile(fmt(clipTime), "durée du montage")}
    ${tile(clipKm.toFixed(1) + " km", "distance couverte")}
    ${tile(s.duration_s ? Math.round(clipTime / s.duration_s * 100) + " %" : "–", "de la session gardée")}</div>`;
  el.innerHTML = html;
}

// ------------------------------------------------------------------ session, sync, export

async function loadSession(id) {
  video.pause();
  st.s = await api("GET", `/api/session/${id}`);
  st.clips = st.s.selections || [];
  st.inPoint = null;
  st.sel = null;
  st.checked = new Set();
  st.horizon = null;
  st.horizonStatus = "";
  st.seg = -1;
  st.tl = { v0: 0, v1: st.s.duration };
  showOffset();
  drawMap(); drawClipsOnMap(); renderClips();
  seek(st.s.candidates[0] ?? 0, false);
  history.replaceState(null, "", "#" + id);
  loadHorizon(id);
  pollExport();
}

function showOffset() {
  const s = st.s;
  $("#offset").textContent = `${s.offset_s >= 0 ? "+" : ""}${s.offset_s.toFixed(1)} s (${s.offset_source}` +
    (s.corr ? `, r=${s.corr.toFixed(2)}` : "") + `) · GPS ${Math.round(s.gps_coverage * 100)} %`;
  const cov = Math.round(s.gps_coverage * 100);
  $("#sync-badge").textContent = cov < 5 ? "absent" : `${s.offset_s >= 0 ? "+" : ""}${s.offset_s.toFixed(1)} s · ${cov} %`;
  const sure = s.offset_source === "manuel" || (s.offset_source === "corrélation" && (s.corr ?? 0) > 0.5);
  $("#sync-dot").className = "dot " + (cov < 30 ? "bad" : sure ? "ok" : "warn");
  $("#sync-dot").title = cov < 30 ? "peu ou pas de GPS" : sure ? "synchronisation fiable" : "synchronisation estimée : à vérifier";
}

async function setOffset(value) {
  const t = now(), clips = st.s.selections;
  const r = await api("POST", `/api/offset/${st.s.id}`, { offset_s: value });
  st.s = { ...r, selections: clips };
  showOffset(); drawMap(); drawClipsOnMap();
  updateMapPos(t);
}

document.querySelectorAll(".sync [data-off]").forEach((b) =>
  b.addEventListener("click", () => setOffset(st.s.offset_s + +b.dataset.off)));
$("#offset-auto").addEventListener("click", () => setOffset(null));

let pollTimer = null;
async function pollExport() {
  clearTimeout(pollTimer);
  if (!st.s) return;
  const j = await api("GET", `/api/export/${st.s.id}`);
  const el = $("#export-status");
  const running = j.state === "running";
  $("#export-preview").disabled = $("#export-final").disabled = $("#hl-start").disabled = running;
  if (running) {
    const kind = { preview: "Aperçu", final: "Export", hyperlapse: "Résumé" }[j.quality] || j.quality;
    el.innerHTML = `<progress value="${j.progress}" max="1"></progress> ${kind} ${Math.round(j.progress * 100)} % · ${j.message}
                    <button id="export-cancel">annuler</button>`;
    $("#export-cancel").onclick = () => api("POST", `/api/export/${st.s.id}`, { cancel: true });
    pollTimer = setTimeout(pollExport, 1000);
  } else if (j.state === "done") {
    el.innerHTML = `✓ <a href="/exports/${encodeURIComponent(j.output)}" target="_blank">${j.output}</a>`;
  } else if (j.state === "error") {
    el.textContent = "⚠ " + j.message.slice(0, 200);
    el.title = j.message;
  } else el.textContent = "";
}

async function startExport(quality) {
  if (!st.clips.length) { $("#export-status").textContent = "Aucun clip sélectionné (I/O)"; return; }
  clearTimeout(saveTimer);
  await api("PUT", `/api/selections/${st.s.id}`, st.clips);
  await api("POST", `/api/export/${st.s.id}`,
            { quality, height: +$("#export-height").value, crf: +$("#export-crf").value });
  pollExport();
}
$("#export-preview").addEventListener("click", () => startExport("preview"));
$("#export-final").addEventListener("click", () => startExport("final"));

// résumé hyperlapse : toute la session, vitesse variable selon l'intérêt
async function hyperlapseInfo() {
  if (!st.s) return;
  const r = await api("POST", `/api/hyperlapse/${st.s.id}`, { duration: +$("#hl-duration").value, preview: true });
  $("#hl-info").textContent = r.output_s < +$("#hl-duration").value - 1
    ? `session courte : ${Math.round(r.output_s)} s au plus`
    : `accéléré de ×${r.slowest_x} (moments forts) à ×${r.fastest_x} (arrêts)`;
}
$("#hl-menu").addEventListener("toggle", (e) => { if (e.target.open) hyperlapseInfo(); });
$("#hl-duration").addEventListener("change", hyperlapseInfo);
$("#hl-start").addEventListener("click", async () => {
  const v = st.view.raw ? { yaw: 0, pitch: -10, fov: 100, roll: 0 } : st.view;
  try {
    await api("POST", `/api/hyperlapse/${st.s.id}`, {
      duration: +$("#hl-duration").value, yaw: v.yaw, pitch: v.pitch, roll: v.roll ?? 0, fov: v.fov,
      horizon: st.view.horizon, height: +$("#export-height").value, crf: +$("#export-crf").value });
  } catch (err) { $("#export-status").textContent = "⚠ export déjà en cours"; return; }
  $("#hl-menu").open = false;
  pollExport();
});

// ------------------------------------------------------------------ commandes

RATES.forEach((r) => {
  const b = document.createElement("button");
  b.textContent = r + "×"; b.dataset.r = r;
  b.addEventListener("click", () => setRate(r));
  $("#rates").appendChild(b);
});
$("#play").addEventListener("click", togglePlay);
$("#skim").addEventListener("change", (e) => { st.skim = e.target.checked; applyRate(); });
$("#view-front").addEventListener("click", () => userView({ yaw: 0, pitch: -10, fov: 100, roll: 0, raw: false }));
$("#view-rider").addEventListener("click", () => userView({ yaw: 180, pitch: 8, fov: 100, roll: 0, raw: false }));
$("#horizon-mode").addEventListener("change", (e) => setView({ horizon: e.target.value }));
$("#view-raw").addEventListener("click", () => setView({ raw: !st.view.raw }));
$("#session").addEventListener("change", (e) => loadSession(e.target.value));

// menus déroulants : un seul ouvert, fermés au clic ailleurs ou sur Échap
document.addEventListener("click", (e) => {
  document.querySelectorAll("details.menu[open]").forEach((d) => { if (!d.contains(e.target)) d.open = false; });
});
document.querySelectorAll("details.menu").forEach((d) => d.addEventListener("toggle", () => {
  if (d.open) document.querySelectorAll("details.menu[open]").forEach((o) => { if (o !== d) o.open = false; });
}));
const helpDlg = $("#help");
$("#help-open").addEventListener("click", () => helpDlg.showModal());
helpDlg.addEventListener("click", (e) => { if (e.target === helpDlg || e.target.closest("[data-close]")) helpDlg.close(); });

function jumpKey(dir) {
  const c = st.clips[st.sel] || activeClip(now());
  if (!c) return;
  const t = now() - c.start, keys = clipKeys(c).map((k) => k.t);
  const target = dir > 0 ? keys.find((x) => x > t + 0.05) : keys.filter((x) => x < t - 0.05).pop();
  if (target !== undefined) { st.viewOverride = false; seek(c.start + target); }
}
$("#key-prev").addEventListener("click", () => jumpKey(-1));
$("#key-next").addEventListener("click", () => jumpKey(1));
$("#key-add").addEventListener("click", () => { const c = activeClip(now()); if (c) upsertKey(c, now()); });

function jumpCandidate(dir) {
  const t = now();
  const list = dir > 0 ? st.s.candidates.filter((c) => c > t + 1) : st.s.candidates.filter((c) => c < t - 3).reverse();
  if (list.length) seek(Math.max(0, list[0] - 5));
}

document.addEventListener("keydown", (e) => {
  if (e.key === "?" && !helpDlg.open) { helpDlg.showModal(); e.preventDefault(); return; }
  if (e.key === "Escape") document.querySelectorAll("details.menu[open]").forEach((d) => (d.open = false));
  if (!st.s || helpDlg.open || e.target.tagName === "SELECT" || e.target.tagName === "INPUT" && e.target.type !== "checkbox" && e.target.type !== "range"
      || e.ctrlKey || e.metaKey || e.altKey) return;
  const k = e.key.toLowerCase();
  const big = e.shiftKey ? 30 : 5;
  const actions = {
    " ": togglePlay, arrowleft: () => seek(now() - big), arrowright: () => seek(now() + big),
    i: markIn, o: markOut, c: quickClip, n: () => jumpCandidate(1), p: () => jumpCandidate(-1),
    "[": () => editEdge("start", now()), "]": () => editEdge("end", now()),
    k: () => { const c = activeClip(now()); if (c) upsertKey(c, now()); },
    ",": () => jumpKey(-1), ".": () => jumpKey(1),
    delete: deleteSelected, l: () => { st.loop = !st.loop; renderEditor(); },
    h: () => setView({ horizon: { auto: "fixe", fixe: "aucun", aucun: "auto" }[st.view.horizon] }),
    f: () => $("#view-front").click(), r: () => $("#view-rider").click(), v: () => $("#view-raw").click(),
    m: () => $("#skim").click(), 0: () => (st.tl = { v0: 0, v1: st.s.duration }),
    escape: () => { st.inPoint = null; updateMarkUI(); },
  };
  if (/^[1-5]$/.test(e.key)) { setRate(RATES[+e.key - 1]); e.preventDefault(); return; }
  if (actions[k]) { actions[k](); e.preventDefault(); }
});

// ------------------------------------------------------------------ boucle d'affichage

let lastClipRender = 0;
function frame(ts) {
  if (st.s) {
    const t = now();
    render();
    drawTimeline();
    applyRate();
    if (!video.paused) followTimeline(t);
    const sc = st.clips[st.sel];
    if (st.loop && sc && !video.paused && (t > sc.end || t < sc.start - 1)) seek(sc.start, true);
    updateMapPos(t);
    const sp = series("speed", t);
    $("#hud").textContent = `${localClock(t)} · ${sp !== null ? sp.toFixed(0) + " km/h" : "— km/h"} · intérêt ${(series("score", t) * 100).toFixed(0)}`;
    $("#clock").textContent = localClock(t);
    $("#tpos").textContent = `${fmt(t)} / ${fmt(st.s.duration)}`;
    if (ts - lastClipRender > 500) { lastClipRender = ts; document.querySelectorAll("#clips li").forEach((li, k) =>
      li.classList.toggle("current", t >= st.clips[k]?.start && t <= st.clips[k]?.end)); }
    const ac = activeClip(t), mode = ac ? clipMode(ac) : st.view.horizon;
    $("#hud").textContent += mode === "auto" ? ` · ${st.horizonStatus || "horizon : auto"}` : ` · horizon : ${mode}`;
    const following = !st.viewOverride || (!video.paused && !st.dragging && !st.autoKey);
    const onKey = ac && clipKeys(ac).some((k) => Math.abs(k.t - (t - ac.start)) < 0.1);
    if (ac && clipKeys(ac).length) $("#hud").textContent += onKey ? " · ◆ sur un point : tout recadrage le modifie"
      : following ? " · ◆ suit les points clés" : " · ◆ recadrage en cours (K pour mémoriser)";
    if (ac !== st.lastActive) { st.lastActive = ac; st.viewOverride = false; }   // entrée dans un clip : on suit ses points
    if (!st.viewOverride) setView({});
  }
  requestAnimationFrame(frame);
}

(async function init() {
  setRate(1);
  setView({});
  await loadSettings();
  const sessions = await api("GET", "/api/sessions");
  const sel = $("#session");
  sessions.forEach((s) => {
    const o = document.createElement("option");
    o.value = s.id;
    o.textContent = `${s.date.slice(6)}/${s.date.slice(4, 6)} ${s.time.slice(0, 2)}:${s.time.slice(2, 4)} · ${fmt(s.duration)}` +
      (s.gps_coverage > 0.5 ? " · GPS" : "") + (s.clips ? ` · ${s.clips} clip(s)` : "");
    sel.appendChild(o);
  });
  const wanted = location.hash.slice(1);
  const first = sessions.find((s) => s.id === wanted) || sessions.reduce((a, b) => (b.duration > a.duration ? b : a));
  sel.value = first.id;
  await loadSession(first.id);
  requestAnimationFrame(frame);
})();
