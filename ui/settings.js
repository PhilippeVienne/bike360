// Réglages globaux enregistrés sur le serveur (/api/settings) : masques fixes floutés sur
// l'image brute (compteur…) et incrustations de télémétrie des exports.
// Partage : loadSettings, saveSettings.

import { st, video } from "./state.js";
import { $, $$, api } from "./util.js";
import { setView } from "./view.js";

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
  $("#mask-edit").textContent = st.maskEdit ? "Terminer masque" : "Masquer…";
  $("#mask-clear").disabled = !st.masks.length;
  $("#mask-clear").textContent = `✕ masques (${st.masks.length})`;
  $("#gl").classList.toggle("masking", st.maskEdit);
}

// incrustations (menu « Incrustations » de l'en-tête)
$$("[data-tel]").forEach((cb) => cb.addEventListener("change", () => {
  st.telemetry = { ...st.telemetry, [cb.dataset.tel]: cb.checked };
  api("PUT", "/api/settings", { telemetry: st.telemetry }).catch(console.error);
}));

// masques : tracés sur la vue brute (le tracé lui-même est géré par viewer.js)
$("#mask-edit").addEventListener("click", () => {
  st.maskEdit = !st.maskEdit;
  if (st.maskEdit) { setView({ raw: true }); video.pause(); } else setView({ raw: false });
  updateMaskUI();
});
$("#mask-clear").addEventListener("click", () => { st.masks = []; saveSettings(); });
