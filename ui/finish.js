// Finition du montage (volet « Finition ») : transitions, titre, musique, volumes, carte de
// fin, enregistrés dans le style du projet ; envoi d'un fichier audio.
// Les champs portent data-style="<clé du style>".
// Partage : renderFinish, useMusic.

import { st } from "./state.js";
import { $, $$, api, esc } from "./util.js";

export async function renderFinish() {
  const style = st.project.style || {};
  const music = await api("GET", "/api/music");
  const sel = $("#fin-music");
  sel.innerHTML = `<option value="">aucune</option>` + music.map((m) => `<option>${esc(m)}</option>`).join("");
  $$("[data-style]").forEach((el) => {
    if (document.activeElement === el) return;   // ne pas écraser le champ en cours de saisie
    if (el.type === "checkbox") el.checked = !!style[el.dataset.style];
    else el.value = style[el.dataset.style] ?? "";
  });
  if (style.duration !== undefined) $('[data-style="duration"]').value = String(+style.duration);
  const parts = [];
  if (style.transition && style.transition !== "aucune") parts.push($('[data-style="transition"]').selectedOptions[0].text);
  if (style.title) parts.push("titre");
  if (style.music) parts.push("musique");
  if (style.end_card) parts.push("carte de fin");
  $("#fin-summary").textContent = parts.length ? "· " + parts.join(", ") : "· aucune";
}

/** Choisit la musique du montage (fichier déjà présent côté serveur). */
export async function useMusic(name) {
  await api("PUT", "/api/project", { style: { music: name } });
  st.project.style = { ...st.project.style, music: name };
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
$("#fin-upload").addEventListener("change", async (e) => {
  const f = e.target.files[0];
  if (!f) return;
  $("#fin-status").textContent = `Envoi de ${f.name}…`;
  const r = await fetch(`/api/music?name=${encodeURIComponent(f.name)}`, { method: "POST", body: f }).then((x) => x.json());
  if (r.error) { $("#fin-status").textContent = "⚠ " + r.error; return; }
  await useMusic(r.name);
  $("#fin-status").textContent = `✓ ${r.name}`;
  e.target.value = "";
  renderFinish();
});
