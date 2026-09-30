// Cadrage affiché (st.view) : réglage de la vue, suivi des points clés du clip traversé,
// enregistrement d'un point clé, analyse d'horizon de la session.
// Partage : activeClip, followsKeyframes, currentView, setView, userView, upsertKey,
// currentViewParams, loadHorizon.

import { st, video, now, DEFAULT_VIEW } from "./state.js";
import { $, api } from "./util.js";
import { clipKeys, clipMode, clipTime, ensureKeyframes, levelMatrix, viewMatrix } from "./geometry.js";
import { saveClips } from "./clips.js";

/** Clip sous la tête de lecture : le sélectionné en priorité, sinon n'importe quel clip traversé. */
export function activeClip(t) {
  const inside = (c) => c && t >= c.start - 0.05 && t <= c.end + 0.05;
  const c = st.clips[st.sel];
  return inside(c) ? c : st.clips.find(inside) || null;
}

/** La vue suit-elle les points clés ? Un réglage manuel ne tient que sur pause (ou pendant
 *  le glisser, ou en mode Auto) : en lecture, on suit toujours les points. */
export const followsKeyframes = () => !st.viewOverride || (!video.paused && !st.dragging && !st.autoKey);

/** Vue affichée : redressement L, matrice écran → caméra M (geometry.view_matrix), clip traversé. */
export function currentView() {
  const v = st.view, t = now(), clip = activeClip(t);
  const L = levelMatrix(t, clip ? clipMode(clip) : v.horizon);
  return { L, M: viewMatrix(L, v), fov: v.fov, t, clip };
}

/** Modifie la vue (angles bornés) et met à jour les commandes qui l'affichent. */
export function setView(p) {
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
export function userView(p) {
  setView(p);
  const c = activeClip(now());
  if (!c) return;
  st.viewOverride = true;
  // Comme l'app Insta360 : sur un point clé (ou en mode Auto), le changement le met à jour directement.
  const onKey = clipKeys(c).some((k) => Math.abs(k.t - (now() - c.start)) < 0.1);
  if (st.autoKey || onKey) { clearTimeout(autoKeyTimer); autoKeyTimer = setTimeout(() => upsertKey(c, now()), 200); }
}

/** Ajoute ou met à jour le point clé du clip c à l'instant t, depuis la vue affichée. */
export function upsertKey(c, t) {
  ensureKeyframes(c);
  c.keyframes = c.keyframes || [];
  const tr = clipTime(c, t);
  const v = { yaw: +st.view.yaw.toFixed(1), pitch: +st.view.pitch.toFixed(1), roll: +st.view.roll.toFixed(1), fov: +st.view.fov.toFixed(1) };
  const near = c.keyframes.find((k) => Math.abs(k.t - tr) < 0.15);
  const same = (k) => k && ["yaw", "pitch", "roll", "fov"].every((f) => Math.abs(k[f] - v[f]) < 0.05);
  const sorted = [...c.keyframes].sort((a, b) => a.t - b.t);
  const before = sorted.filter((k) => k.t < tr).pop(), after = sorted.find((k) => k.t > tr);
  if (near) Object.assign(near, v);
  else if (same(before) && (!after || same(after))) { st.viewOverride = false; return; }   // rien de nouveau
  // un point inséré hérite de la courbe du segment ; ajouté à la fin, il est linéaire (comme l'app)
  else c.keyframes.push({ t: tr, ...v, curve: before && after ? before.curve : "linear" });
  c.keyframes.sort((a, b) => a.t - b.t);
  st.viewOverride = false;
  saveClips(c);
}

/** Cadrage à enregistrer dans un clip (la vue brute n'a pas de sens : vue avant par défaut). */
export function currentViewParams() {
  const v = st.view;
  const view = v.raw ? { ...DEFAULT_VIEW }
    : { yaw: +v.yaw.toFixed(1), pitch: +v.pitch.toFixed(1), fov: +v.fov.toFixed(1), roll: +v.roll.toFixed(1) };
  return { ...view, horizon: v.horizon };
}

/** Horizon mesuré dans l'image : chargé quand l'analyse est finie, sinon on réinterroge. */
let horizonTimer = null;
export async function loadHorizon(id) {
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
