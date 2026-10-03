// Finition du montage (volet « Finition ») : transitions, titre, carte de fin, enregistrés dans
// le style du projet. Les champs portent data-style="<clé du style>". La musique et les volumes
// se règlent dans l'éditeur de montage (mt-editor.js).
// Partage : renderFinish.

import { st } from "./state.js";
import { $, $$, api } from "./util.js";

export async function renderFinish() {
  const style = st.project.style || {};
  $$("[data-style]").forEach((el) => {
    if (document.activeElement === el) return;   // ne pas écraser le champ en cours de saisie
    if (el.type === "checkbox") el.checked = !!style[el.dataset.style];
    else el.value = style[el.dataset.style] ?? "";
  });
  if (style.duration !== undefined) $('[data-style="duration"]').value = String(+style.duration);
  const parts = [];
  if (style.transition && style.transition !== "aucune") parts.push($('[data-style="transition"]').selectedOptions[0].text);
  if (style.title) parts.push("titre");
  if (style.audio_tracks?.length) parts.push(style.audio_tracks.length > 1 ? `${style.audio_tracks.length} pistes audio` : "musique");
  if (style.end_card) parts.push("carte de fin");
  $("#fin-summary").textContent = parts.length ? "· " + parts.join(", ") : "· aucune";
}

let styleTimer = null;
$("#mt-finish").addEventListener("input", (e) => {
  const key = e.target.dataset.style;
  if (!key) return;
  const v = e.target.type === "checkbox" ? e.target.checked
    : e.target.type === "range" || key === "duration" ? +e.target.value : e.target.value;
  st.project.style = { ...st.project.style, [key]: v };
  clearTimeout(styleTimer);
  styleTimer = setTimeout(async () => { await api("PUT", "/api/project", { style: { [key]: v } }); renderFinish(); }, 400);
});
