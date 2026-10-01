// Session affichée : chargement (clips, carte, horizon, zones…) et synchronisation de
// l'horloge caméra avec le GPS GeoRide (Réglages → GPS ; pastille GPS de l'étape ②).
// Partage : loadSession.

import { st, video, now } from "./state.js";
import { $, $$, api, dayLabel, fmt, hhmm } from "./util.js";
import { loadHorizon } from "./view.js";
import { seek } from "./playback.js";
import { loadPrivacyTracks } from "./zones.js";
import { drawClipsOnMap, drawMap, updateMapPos } from "./map.js";
import { renderClips } from "./clips.js";
import { pollExport } from "./export.js";
import { updateSteps } from "./steps.js";

export async function loadSession(id) {
  video.pause();
  st.s = await api("GET", `/api/session/${id}`);
  st.clips = st.s.selections || [];
  st.pvTracks = {};
  loadPrivacyTracks();
  st.inPoint = null;
  st.sel = null;
  st.checked = new Set();
  st.horizon = null;
  st.horizonStatus = "";
  st.seg = -1;
  st.tl = { v0: 0, v1: st.s.duration };
  const info = st.sessions.find((x) => x.id === id);
  const label = `${dayLabel(st.s.date)} · ${hhmm(st.s)} · ${fmt(st.s.duration)}` +
    (info && info.parts > 1 ? ` · boucle de ${info.parts} fichiers` : "");
  $$(".session-label").forEach((el) => (el.textContent = label));
  $$(".file-card").forEach((c) => c.classList.toggle("current", c.dataset.sid === id));
  const pick = $("#session-pick");
  if (![...pick.options].some((o) => o.value === id)) pick.add(new Option(`${hhmm(st.s)} · ${fmt(st.s.duration)} (hors projet)`, id));
  pick.value = id;
  updateSteps();
  showOffset();
  drawMap(); drawClipsOnMap(); renderClips();
  seek(st.s.candidates[0] ?? 0, false);
  history.replaceState(null, "", "#" + id);
  loadHorizon(id);
  pollExport();
}

// ------------------------------------------------------------------ synchro caméra ↔ GPS

/** Décalage appliqué, sa source et la couverture GPS (menu et pastille de l'en-tête). */
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

/** Nouveau décalage (secondes), ou null pour revenir au calcul automatique. */
async function setOffset(value) {
  const t = now(), clips = st.s.selections;
  const r = await api("POST", `/api/offset/${st.s.id}`, { offset_s: value });
  st.s = { ...r, selections: clips };
  showOffset(); drawMap(); drawClipsOnMap();
  updateMapPos(t);
}

$$(".sync [data-off]").forEach((b) =>
  b.addEventListener("click", () => setOffset(st.s.offset_s + +b.dataset.off)));
$("#offset-auto").addEventListener("click", () => setOffset(null));
