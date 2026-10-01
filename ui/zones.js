// Rectangles sur la vue : zones de confidentialité superposées à l'aperçu (calque #zones),
// tracé à la main d'une zone à flouter (élément raté par la détection) ou d'un compagnon
// à suivre (le serveur suit l'objet et écrit les points clés du clip).
// Partage : drawZones, loadPrivacyTracks, setZoneEdit, sendZone.

import { st, video } from "./state.js";
import { $, api, apiOrError, css } from "./util.js";
import { regionsAt, sphereToBox } from "./geometry.js";
import { currentView, setView } from "./view.js";
import { seek } from "./playback.js";
import { renderClips } from "./clips.js";
import { drawClipsOnMap } from "./map.js";
import { privacyShown, renderPrivacy } from "./privacy.js";

const gl = $("#gl"), zc = $("#zones"), zctx = zc.getContext("2d");

/** Dessine les zones floutées (rouge) et le rectangle en cours de tracé, alignés sur la vue WebGL. */
export function drawZones() {
  const show = st.zoneEdit || privacyShown();
  const cw = gl.clientWidth, ch = gl.clientHeight, dpr = devicePixelRatio;
  if (zc.width !== cw * dpr || zc.height !== ch * dpr) { zc.width = cw * dpr; zc.height = ch * dpr; zc.style.width = cw + "px"; zc.style.height = ch + "px"; }
  zctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  zctx.clearRect(0, 0, cw, ch);
  if (!show || !st.s || st.view.raw) return;
  const { M, fov, t } = currentView();
  const clips = st.clips.filter((c) => t >= c.start - 1 && t <= c.end + 1);
  for (const c of clips) {
    for (const r of regionsAt(st.pvTracks[c.id] || [], t)) {
      const b = sphereToBox(r.d, r.ax, r.ay, M, fov, cw, ch);
      if (!b) continue;
      zctx.setLineDash(r.enabled ? [6, 4] : [2, 4]);
      zctx.lineWidth = 2;
      zctx.strokeStyle = r.enabled ? "#ff4d4f" : "rgba(200,200,200,.7)";
      if (r.enabled) { zctx.fillStyle = "rgba(255,77,79,.18)"; zctx.fillRect(...b); }
      zctx.strokeRect(...b);
    }
  }
  if (st.zoneDraft) {
    const z = st.zoneDraft;
    zctx.setLineDash([]); zctx.lineWidth = 2; zctx.strokeStyle = css("--accent");
    zctx.strokeRect(z.x * cw, z.y * ch, z.w * cw, z.h * ch);
  }
}

/** Charge les zones de confidentialité de la session affichée (aperçu sur la vidéo). */
export async function loadPrivacyTracks() {
  if (!st.s) return;
  const id = st.s.id, r = await api("GET", `/api/privacy/${id}`);
  if (st.s && st.s.id === id) st.pvTracks = r;
}

/** Active le tracé d'un rectangle sur la vidéo : zone à flouter (« privacy ») ou compagnon à suivre (« follow »). */
export function setZoneEdit(on, mode = "privacy") {
  st.zoneEdit = on;
  st.zoneMode = mode;
  $("#pv-draw").classList.toggle("active", on && mode === "privacy");
  $("#pv-draw").textContent = on && mode === "privacy" ? "Annuler le tracé" : "＋ Zone à la main";
  gl.classList.toggle("zoning", on);
  // bandeau sur la vidéo : consigne et « Annuler » (au doigt, pas d'Échap)
  $("#zone-banner").hidden = !on;
  $("#zone-banner-text").textContent = mode === "privacy" ? "Trace un rectangle autour de l'élément à flouter."
    : "Trace un rectangle autour du motard ou de la voiture à suivre.";
  if (on) {
    video.pause();
    if (st.view.raw) setView({ raw: false });
    if (mode === "privacy") $("#pv-status").textContent = "Trace un rectangle sur la vidéo autour de l'élément à flouter.";
    else $("#ed-follow").textContent = "Trace un rectangle autour du motard ou de la voiture à suivre.";
  }
}
$("#zone-cancel").addEventListener("click", () => setZoneEdit(false));

/** Rectangle tracé (0..1 dans la vue) + vue qui l'a vu : de quoi le retrouver sur la sphère côté serveur. */
function zoneRequest(z, L, t, clip) {
  const v = st.view;
  return { sid: st.s.id, clip: clip.id, t, box: [z.x, z.y, z.w, z.h],
           aspect: gl.clientWidth / gl.clientHeight,
           view: { yaw: v.yaw, pitch: v.pitch, roll: v.roll || 0, fov: v.fov, level: L } };
}

/** Fin du tracé : envoie la zone au serveur (floutage) ou lance le suivi du compagnon. */
export async function sendZone(z) {
  const { L, t, clip } = currentView();
  const mode = st.zoneMode;
  setZoneEdit(false);
  if (mode === "follow") return sendFollow(z, L, t, clip);
  if (!clip) { $("#pv-status").textContent = "⚠ Place la tête de lecture dans un clip."; return; }
  const r = await apiOrError("POST", "/api/privacy/manual", { ...zoneRequest(z, L, t, clip), follow: $("#pv-follow").checked });
  if (r.error) { $("#pv-status").textContent = "⚠ " + r.error; return; }
  renderPrivacy();
}

// suivi d'un compagnon : le serveur suit l'objet et remplace les points clés du clip
async function sendFollow(z, L, t, clip) {
  if (!clip) { $("#ed-follow").textContent = "⚠ Place la tête de lecture dans le clip."; return; }
  const r = await apiOrError("POST", "/api/follow", zoneRequest(z, L, t, clip));
  if (r.error) { $("#ed-follow").textContent = "⚠ " + r.error; return; }
  pollFollow(st.s.id, clip.id);
}
async function pollFollow(sid, clipId) {
  const j = await api("GET", "/api/export/follow");
  if (j.state === "running") {
    $("#ed-follow").textContent = `🎯 ${Math.round((j.progress || 0) * 100)} % · ${j.message}`;
    return setTimeout(() => pollFollow(sid, clipId), 1000);
  }
  if (j.state === "error") { $("#ed-follow").textContent = "⚠ " + j.message; return; }
  if (st.s && st.s.id === sid) {   // points clés écrits par le serveur : on recharge les clips
    const s2 = await api("GET", `/api/session/${sid}`);
    st.clips = s2.selections || [];
    const k = st.clips.findIndex((c) => c.id === clipId);
    st.sel = k >= 0 ? k : null;
    st.viewOverride = false;
    renderClips(); drawClipsOnMap();
    if (k >= 0) seek(st.clips[k].start);
  }
  $("#ed-follow").textContent = "✓ " + j.message;
}
