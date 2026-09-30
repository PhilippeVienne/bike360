// Visionneuse WebGL : le .lrv (double fisheye) est reprojeté en vue plane selon st.view,
// redressé comme à l'export, avec les masques fixes floutés. Gère aussi les gestes sur
// l'image : orienter (glisser), champ (molette, pincement), rotation (Ctrl+glisser),
// tracé des masques et des zones.
// Partage : render (appelé à chaque image par la boucle d'affichage de main.js).

import { st, video, now, LENS_FOV } from "./state.js";
import { $ } from "./util.js";
import { clipKeys, clipMode, clipViewAt, levelMatrix } from "./geometry.js";
import { activeClip, followsKeyframes, userView } from "./view.js";
import { drawZones, sendZone } from "./zones.js";
import { saveSettings } from "./settings.js";
import { updateFrameGuide } from "./export.js";

const canvas = $("#gl");
const gl = canvas.getContext("webgl", { antialias: false });

// ------------------------------------------------------------------ shaders

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

/** Compile le programme, crée le quad plein écran et la texture vidéo ; renvoie les uniformes. */
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

// ------------------------------------------------------------------ rendu d'une image

export function render() {
  const w = canvas.clientWidth * devicePixelRatio | 0, h = canvas.clientHeight * devicePixelRatio | 0;
  if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
  gl.viewport(0, 0, w, h);
  if (video.readyState >= 2) gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGB, gl.RGB, gl.UNSIGNED_BYTE, video);
  // Dans un clip à points clés, la vue suit l'interpolation (sauf réglage manuel en cours).
  if (st.s && followsKeyframes()) {
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
  drawZones();
  const ms = st.maskDraft ? [...st.masks, st.maskDraft] : st.masks;
  const flat = new Float32Array(32);
  ms.slice(0, 8).forEach((m, i) => flat.set([m.x, m.y, m.w, m.h], i * 4));
  gl.uniform4fv(U.masks, flat);
  gl.uniform1i(U.nMasks, Math.min(8, ms.length));
  gl.uniform1i(U.maskEdit, st.maskEdit ? 1 : 0);
  gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
}

// ------------------------------------------------------------------ gestes sur l'image

{
  let drag = null;
  const cv = canvas;
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
  const canvasPos = (e) => {   // position dans le canvas (0..1)
    const r = cv.getBoundingClientRect();
    return { x: Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)), y: Math.max(0, Math.min(1, (e.clientY - r.top) / r.height)) };
  };
  const rectBetween = (a, b) => ({ x: Math.min(a.x, b.x), y: Math.min(a.y, b.y), w: Math.abs(a.x - b.x), h: Math.abs(a.y - b.y) });
  const fingers = new Map();   // pincement à deux doigts = champ (zoom)
  const spread = () => { const [a, b] = [...fingers.values()]; return Math.hypot(a.x - b.x, a.y - b.y) || 1; };

  cv.addEventListener("pointerdown", (e) => {
    fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (fingers.size === 2 && !st.zoneEdit && !st.maskEdit) {
      drag = { pinch: { d: spread(), fov: st.view.fov } };
      st.dragging = true;
      return;
    }
    drag = { x: e.clientX, y: e.clientY, q: texCoord(e), rotate: e.ctrlKey || e.metaKey, a: angleAt(e), z: st.zoneEdit ? canvasPos(e) : null };
    st.dragging = true;
    cv.classList.toggle("rotating", drag.rotate);
    cv.setPointerCapture(e.pointerId);
  });
  const release = (e) => {
    fingers.delete(e.pointerId);
    if (drag && drag.pinch) { if (!fingers.size) { drag = null; st.dragging = false; } return; }
    cv.classList.remove("rotating");
    st.dragging = false;
    if (drag && drag.z) {   // fin du tracé d'une zone (floutage ou suivi)
      const z = st.zoneDraft;
      st.zoneDraft = null; drag = null;
      if (z && z.w > 0.005 && z.h > 0.005) sendZone(z);
      return;
    }
    const m = st.maskDraft;   // fin du tracé d'un masque fixe
    st.maskDraft = null; drag = null;
    if (m && m.w > .005 && m.h > .01) { st.masks.push(m); saveSettings(); }
  };
  cv.addEventListener("pointerup", release);
  cv.addEventListener("pointercancel", release);
  cv.addEventListener("pointermove", (e) => {
    if (fingers.has(e.pointerId)) fingers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (drag && drag.pinch) {
      if (fingers.size === 2) userView({ fov: drag.pinch.fov * drag.pinch.d / spread() });
      return;
    }
    if (drag && drag.z) { st.zoneDraft = rectBetween(canvasPos(e), drag.z); return; }
    if (drag && st.maskEdit) { st.maskDraft = rectBetween(texCoord(e), drag.q); return; }
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

// Vue 16:9 ajustée à l'espace disponible (letterbox).
new ResizeObserver(() => {
  const stage = $(".stage"), W = stage.clientWidth, H = stage.clientHeight;
  let w = W, h = W * 9 / 16;
  if (h > H) { h = H; w = H * 16 / 9; }
  canvas.style.width = Math.floor(w) + "px";
  canvas.style.height = Math.floor(h) + "px";
  updateFrameGuide();
}).observe($(".stage"));
