// Étape ④ Exporter, réglages communs (destination, résolution, qualité) et exports de la
// session affichée : cadre de la destination sur l'aperçu, aperçu 720p / export des clips,
// résumé hyperlapse et suivi de la progression (avec annulation).
// Partage : exportSettings, updateFrameGuide, pollExport.

import { st, defaultView } from "./state.js";
import { $, api, exportLink, storageGet, storageSet } from "./util.js";
import { saveClipsNow } from "./clips.js";
import { onStep, setJob } from "./steps.js";

/** Destination et encodage choisis à l'étape ④ (communs aux clips, au résumé et au montage). */
export const exportSettings = () => ({ format: $("#export-format").value,
                                       height: +$("#export-height").value, crf: +$("#export-crf").value });

// Destination de l'export : en carré et en vertical, seule la largeur centrale (un carré de la
// hauteur de l'aperçu) est gardée ; le vertical ajoute du champ au-dessus et au-dessous.
export function updateFrameGuide() {
  const fmt = $("#export-format").value, g = $("#frame-guide");
  g.hidden = fmt !== "carre" && fmt !== "vertical";
  $("#export-height").disabled = fmt !== "standard";
  if (g.hidden) return;
  const h = $("#gl").clientHeight;
  g.style.width = g.style.height = h + "px";
  g.firstChild.textContent = fmt === "carre" ? "cadre 1:1" : "largeur 9:16 · + champ en haut et en bas";
}
$("#export-format").addEventListener("change", () => {
  updateFrameGuide();
  storageSet("exportFormat", $("#export-format").value);
});
$("#export-format").value = storageGet("exportFormat") || "standard";
updateFrameGuide();

// ------------------------------------------------------------------ clips de la session

let pollTimer = null;
export async function pollExport() {
  clearTimeout(pollTimer);
  if (!st.s) return;
  const j = await api("GET", `/api/export/${st.s.id}`);
  const el = $("#export-status");
  const running = j.state === "running";
  $("#export-preview").disabled = $("#export-final").disabled = $("#hl-start").disabled = running;
  const kind = { preview: "Aperçu", final: "Export", hyperlapse: "Résumé" }[j.quality] || j.quality;
  setJob("session", running ? `${kind} ${Math.round(j.progress * 100)} %` : null);
  if (running) {
    el.innerHTML = `<progress value="${j.progress}" max="1"></progress> ${kind} ${Math.round(j.progress * 100)} % · ${j.message}
                    <button id="export-cancel">annuler</button>`;
    if (j.warning) el.insertAdjacentHTML("beforeend", `<span class="warn">⚠ ${j.warning}</span>`);
    $("#export-cancel").onclick = () => api("POST", `/api/export/${st.s.id}`, { cancel: true });
    pollTimer = setTimeout(pollExport, 1000);
  } else if (j.state === "done") {
    el.innerHTML = exportLink(j.output);
    if (j.warning) el.insertAdjacentHTML("beforeend", `<span class="warn">⚠ ${j.warning}</span>`);
  } else if (j.state === "error") {
    el.textContent = "⚠ " + j.message.slice(0, 200);
    el.title = j.message;
  } else el.textContent = "";
}

async function startExport(quality) {
  if (!st.clips.length) { $("#export-status").textContent = "Aucun clip dans cette session (étape ②)."; return; }
  await saveClipsNow();
  await api("POST", `/api/export/${st.s.id}`, { quality, ...exportSettings() });
  pollExport();
}
$("#export-preview").addEventListener("click", () => startExport("preview"));
$("#export-final").addEventListener("click", () => startExport("final"));

// ------------------------------------------------------------------ résumé hyperlapse : toute la session, vitesse variable selon l'intérêt

async function hyperlapseInfo() {
  if (!st.s) return;
  const r = await api("POST", `/api/hyperlapse/${st.s.id}`, { duration: +$("#hl-duration").value, preview: true });
  $("#hl-info").textContent = r.output_s < +$("#hl-duration").value - 1
    ? `session courte : ${Math.round(r.output_s)} s au plus`
    : `accéléré de ×${r.slowest_x} (moments forts) à ×${r.fastest_x} (arrêts)`;
}
onStep((s) => { if (s === "export") { hyperlapseInfo(); updateFrameGuide(); } });
$("#hl-duration").addEventListener("change", hyperlapseInfo);
$("#hl-start").addEventListener("click", async () => {
  const v = st.view.raw ? defaultView() : st.view;   // cadrage de l'aperçu
  try {
    await api("POST", `/api/hyperlapse/${st.s.id}`, {
      duration: +$("#hl-duration").value, yaw: v.yaw, pitch: v.pitch, roll: v.roll ?? 0, fov: v.fov,
      horizon: st.view.horizon, ...exportSettings() });
  } catch (err) { $("#export-status").textContent = "⚠ export déjà en cours"; return; }
  pollExport();
});
