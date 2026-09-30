// Frise (timeline) : métriques de la session (intérêt, vitesse, virages, altitude,
// vibrations), clips, points clés, moments forts et tête de lecture, sur une fenêtre
// st.tl zoomable. Gestes : clic = aller à, glisser un bord de clip ou un ◆, balayage et
// pincement au doigt, molette = zoom ; barre de défilement dessous.
// Partage : drawTimeline, followTimeline, showWholeSession.

import { st, now, series } from "./state.js";
import { $, css, localClock } from "./util.js";
import { clipKeys, clipTime, ensureKeyframes } from "./geometry.js";
import { seek } from "./playback.js";
import { renderClips, saveClips, setClipEdge } from "./clips.js";
import { renderEditor } from "./clip-editor.js";

const tl = $("#timeline");
const tctx = tl.getContext("2d");
const TRACKS = [   // [clé, libellé, hauteur, couleur, type]
  ["score", "intérêt", 22, null, "heat"],
  ["speed", "vitesse", 38, "--speed", "line"],
  ["turn", "virages", 30, "--turn", "bars"],
  ["alt", "altitude", 28, "--alt", "area"],
  ["vib", "vibrations", 22, "--muted", "line"],
];
const LABEL_W = 70;          // colonne des libellés à gauche
const KEY_LANE = 18;         // hauteur de la ligne « ◆ points » en haut de la frise
const KEY_Y = 16 + KEY_LANE / 2;
const MIN_SPAN = 30;         // zoom maximal : 30 s visibles

const xOf = (t) => LABEL_W + (t - st.tl.v0) / (st.tl.v1 - st.tl.v0) * (tl.clientWidth - LABEL_W);
const tOf = (x) => st.tl.v0 + (x - LABEL_W) / (tl.clientWidth - LABEL_W) * (st.tl.v1 - st.tl.v0);

// ------------------------------------------------------------------ fenêtre visible

/** Largeur de fenêtre bornée entre MIN_SPAN et la durée de la session. */
const clampSpan = (span) => Math.max(MIN_SPAN, Math.min(st.s.duration, span));

/** Affiche la fenêtre [v0, v0 + span], décalée pour rester dans la session. */
function setWindow(v0, span) {
  v0 = Math.max(0, Math.min(st.s.duration - span, v0));
  st.tl = { v0, v1: v0 + span };
}
export function showWholeSession() { st.tl = { v0: 0, v1: st.s.duration }; }

/** Fait défiler la frise pour garder t visible (pendant la lecture, après un saut). */
export function followTimeline(t) {
  const span = st.tl.v1 - st.tl.v0;
  if (span >= st.s.duration) return;
  if (t < st.tl.v0 || t > st.tl.v1) setWindow(t - span * 0.2, span);
}

const panTimeline = (v0) => setWindow(v0, st.tl.v1 - st.tl.v0);
const zoomTimeline = (k, center) => { const span = clampSpan((st.tl.v1 - st.tl.v0) * k); setWindow(center - span / 2, span); };

// barre de défilement : partie visible de la frise ; glisser = défiler, appui ailleurs = y centrer
function updateTlScroll() {
  if (!st.s) return;
  const n = st.s.duration, th = $("#tl-thumb");
  th.style.left = (st.tl.v0 / n * 100) + "%";
  th.style.width = ((st.tl.v1 - st.tl.v0) / n * 100) + "%";
}
{
  const bar = $("#tl-scroll");
  let grab = null;
  const tAt = (e) => (e.clientX - bar.getBoundingClientRect().left) / bar.clientWidth * st.s.duration;
  bar.addEventListener("pointerdown", (e) => {
    if (!st.s) return;
    const t = tAt(e);
    if (t < st.tl.v0 || t > st.tl.v1) panTimeline(t - (st.tl.v1 - st.tl.v0) / 2);   // appui hors du curseur
    grab = { dt: t - st.tl.v0 };
    bar.classList.add("dragging");
    bar.setPointerCapture(e.pointerId);
  });
  bar.addEventListener("pointermove", (e) => { if (grab) panTimeline(tAt(e) - grab.dt); });
  const end = () => { grab = null; bar.classList.remove("dragging"); };
  bar.addEventListener("pointerup", end);
  bar.addEventListener("pointercancel", end);
  $("#tl-zoom-in").addEventListener("click", () => st.s && zoomTimeline(0.5, now()));
  $("#tl-zoom-out").addEventListener("click", () => st.s && zoomTimeline(2, (st.tl.v0 + st.tl.v1) / 2));
  $("#tl-all").addEventListener("click", () => st.s && showWholeSession());
}

// ------------------------------------------------------------------ dessin

function heat(v) {  // 0 → sombre, 1 → ambre vif
  const a = Math.max(0, Math.min(1, v));
  return `rgba(245,165,36,${(a * a).toFixed(3)})`;
}

export function drawTimeline() {
  updateTlScroll();
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

  // pistes des métriques
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
  // moments forts (candidats)
  tctx.fillStyle = css("--accent");
  st.s.candidates.forEach((t) => {
    const x = xOf(t);
    tctx.beginPath(); tctx.moveTo(x - 5, 12); tctx.lineTo(x + 5, 12); tctx.lineTo(x, 19); tctx.fill();
  });
  // limites des segments (fichiers)
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

// ------------------------------------------------------------------ gestes

/** Point clé sous le pointeur (ligne ◆, ±6 px). */
function keyAt(x, y) {
  if (Math.abs(y - KEY_Y) > 8) return null;
  for (let k = 0; k < st.clips.length; k++) {
    const keys = clipKeys(st.clips[k]);
    for (let i = 0; i < keys.length; i++) if (Math.abs(xOf(st.clips[k].start + keys[i].t) - x) <= 6) return { k, key: keys[i] };
  }
  return null;
}
/** Bord de clip sous la souris (±6 px) : on privilégie le clip sélectionné. */
function edgeAt(x) {
  const order = st.clips.map((c, k) => k).sort((a, b) => (b === st.sel) - (a === st.sel));
  for (const k of order) for (const edge of ["start", "end"])
    if (Math.abs(xOf(st.clips[k][edge]) - x) <= 6) return { k, edge };
  return null;
}

// Glisser en cours : {pinch}, {scrub}, {k, key} (point clé) ou {k, edge} (bord de clip).
// Il est effacé juste après le relâchement pour que le « click » qui suit ne déplace pas la lecture.
let tlDrag = null;
const clearDragSoon = () => setTimeout(() => (tlDrag = null), 0);
// Pointeurs (souris et doigts) : bords de clip, points clés, balayage (glisser dans le vide),
// pincement à deux doigts = zoom de la frise.
const tlTouches = new Map();
tl.addEventListener("pointerdown", (e) => {
  tlTouches.set(e.pointerId, e.clientX);
  if (tlTouches.size === 2) {   // début d'un pincement : on abandonne le glisser en cours
    const [a, b] = [...tlTouches.values()];
    tlDrag = { pinch: { d: Math.abs(a - b) || 1, span: st.tl.v1 - st.tl.v0, mid: tOf((a + b) / 2 - tl.getBoundingClientRect().left) } };
    return;
  }
  if (!st.s || e.offsetX <= LABEL_W) return;
  tl.setPointerCapture(e.pointerId);
  const kh = keyAt(e.offsetX, e.offsetY);
  if (kh) {
    const c = st.clips[kh.k];
    ensureKeyframes(c);
    tlDrag = { k: kh.k, key: c.keyframes.find((x) => x.t === kh.key.t), moved: false };
    st.sel = kh.k; renderClips(); e.preventDefault();
    return;
  }
  const hit = edgeAt(e.offsetX);
  if (hit) { tlDrag = { ...hit, moved: false }; st.sel = hit.k; renderEditor(); e.preventDefault(); return; }
  if (e.pointerType !== "mouse") tlDrag = { scrub: true, moved: false };   // au doigt : balayage
});
tl.addEventListener("pointermove", (e) => {
  if (tlTouches.has(e.pointerId)) tlTouches.set(e.pointerId, e.clientX);
  if (!tlDrag) return;
  const r = tl.getBoundingClientRect();
  if (tlDrag.pinch) {
    if (tlTouches.size < 2) return;
    const [a, b] = [...tlTouches.values()], p = tlDrag.pinch;
    const span = clampSpan(p.span * p.d / Math.max(10, Math.abs(a - b)));
    setWindow(p.mid - span / 2, span);
    return;
  }
  if (tlDrag.scrub) { tlDrag.moved = true; seek(tOf(Math.max(LABEL_W, e.clientX - r.left))); return; }
  const c = st.clips[tlDrag.k], t = tOf(e.clientX - r.left);
  if (tlDrag.key) {   // déplacement d'un point clé dans le clip
    tlDrag.key.t = clipTime(c, t);
    c.keyframes.sort((a, b) => a.t - b.t);
    tlDrag.moved = true;
    st.viewOverride = false;
    seek(c.start + tlDrag.key.t);
    renderEditor();
    return;
  }
  setClipEdge(c, tlDrag.edge, t);
  tlDrag.moved = true;
  seek(c[tlDrag.edge]);                 // on voit l'image du bord pendant le réglage
  renderEditor();
});
const tlRelease = (e) => {
  tlTouches.delete(e.pointerId);
  if (!tlDrag) return;
  if (tlDrag.pinch) { if (!tlTouches.size) clearDragSoon(); return; }
  if (tlDrag.scrub) {   // simple appui au doigt : le « click » qui suit place la lecture
    if (tlDrag.moved) clearDragSoon(); else tlDrag = null;
    return;
  }
  const c = st.clips[tlDrag.k];
  if (tlDrag.key && !tlDrag.moved) { st.viewOverride = false; seek(c.start + tlDrag.key.t); }
  if (tlDrag.moved) saveClips(c);
  clearDragSoon();
};
tl.addEventListener("pointerup", tlRelease);
tl.addEventListener("pointercancel", tlRelease);
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
tl.addEventListener("wheel", (e) => {   // zoom autour du pointeur
  if (!st.s) return;
  e.preventDefault();
  const t = tOf(Math.max(LABEL_W, e.offsetX));
  const k = e.deltaY > 0 ? 1.25 : 0.8;
  const span = clampSpan((st.tl.v1 - st.tl.v0) * k);
  setWindow(t - (t - st.tl.v0) * span / (st.tl.v1 - st.tl.v0), span);
}, { passive: false });
