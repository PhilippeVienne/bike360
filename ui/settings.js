// Réglages globaux enregistrés sur le serveur (/api/settings) : masques fixes floutés sur
// l'image brute (compteur…, depuis Réglages, tracés sur la vidéo de l'étape ②) et
// incrustations de télémétrie des exports (étape ④).
// Partage : loadSettings, saveSettings.

import { st, video } from "./state.js";
import { $, $$, api } from "./util.js";
import { setView } from "./view.js";
import { goStep } from "./steps.js";

export async function loadSettings() {
  const cfg = await api("GET", "/api/settings");
  st.masks = cfg.masks || [];
  st.telemetry = cfg.telemetry || {};
  $$("[data-tel]").forEach((cb) => (cb.checked = !!st.telemetry[cb.dataset.tel]));
  updateMaskUI();
}

export function saveSettings() {
  updateMaskUI();
  api("PUT", "/api/settings", { masks: st.masks, telemetry: st.telemetry }).catch(console.error);
}

function updateMaskUI() {
  $("#mask-edit").classList.toggle("active", st.maskEdit);
  $("#mask-edit").textContent = st.maskEdit ? "Terminer les masques" : "Tracer des masques…";
  $("#mask-clear").disabled = !st.masks.length;
  $("#mask-clear").textContent = `Effacer les masques (${st.masks.length})`;
  $("#mask-count").textContent = st.masks.length ? `${st.masks.length} zone(s) floutée(s) sur toutes les vidéos` : "aucun masque";
  $("#gl").classList.toggle("masking", st.maskEdit);
  $("#mask-banner").hidden = !st.maskEdit;
}

// incrustations (étape ④)
$$("[data-tel]").forEach((cb) => cb.addEventListener("change", () => {
  st.telemetry = { ...st.telemetry, [cb.dataset.tel]: cb.checked };
  api("PUT", "/api/settings", { telemetry: st.telemetry }).catch(console.error);
}));

// masques : tracés sur la vue brute (le tracé lui-même est géré par viewer.js)
function toggleMaskEdit() {
  st.maskEdit = !st.maskEdit;
  if (st.maskEdit) {   // tracé sur la vidéo de l'étape ② : on ferme les Réglages
    $("#settings").close();
    goStep("cut");
    setView({ raw: true }); video.pause();
  } else setView({ raw: false });
  updateMaskUI();
}
$("#mask-edit").addEventListener("click", toggleMaskEdit);
$("#mask-done").addEventListener("click", toggleMaskEdit);
$("#mask-clear").addEventListener("click", () => { st.masks = []; saveSettings(); });
