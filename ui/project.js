// Panneau Projet, onglet Fichiers : sessions groupées par jour (case « projet » pour les
// inclure au montage), ouverture d'une session ; menu « Dossiers » de l'en-tête (dossiers
// de vidéos analysés).
// Partage : refreshSessions.

import { st } from "./state.js";
import { $, api, apiOrError, dayLabel, esc, fmt, hhmm } from "./util.js";
import { loadSession } from "./session.js";
import { renderMontage } from "./montage.js";
import { closeDrawer, isNarrow } from "./panels.js";

/** Recharge la liste des sessions et le projet (clips du montage), puis les deux onglets. */
export async function refreshSessions() {
  [st.sessions, st.project] = await Promise.all([api("GET", "/api/sessions"), api("GET", "/api/project")]);
  renderFiles();
  renderMontage();
  return st.sessions;
}

function renderFiles() {
  const list = $("#file-list"), inProj = new Set(st.project.sessions), all = $("#show-all").checked;
  list.innerHTML = "";
  let day = null, hidden = 0;
  st.sessions.forEach((s) => {
    const inside = inProj.has(s.id);
    if (!inside && !all) { hidden++; return; }
    if (s.date !== day) {
      day = s.date;
      list.insertAdjacentHTML("beforeend", `<div class="day-head">${dayLabel(s.date)}</div>`);
    }
    const folder = (s.folder || "").split("/").filter(Boolean).pop() || "";
    const card = document.createElement("div");
    card.className = "file-card" + (st.s && st.s.id === s.id ? " current" : "") + (inside ? "" : " out");
    card.dataset.sid = s.id;
    card.innerHTML = `<img loading="lazy" alt="" src="/thumb/${s.id}.jpg">
      <label class="fc-check" title="Inclure ce fichier dans le projet"><input type="checkbox" ${inside ? "checked" : ""}> projet</label>
      <div class="fc-info"><strong>${hhmm(s)}</strong> · ${fmt(s.duration)}${s.parts > 1 ? `<span class="badge" title="Enregistrement en boucle : ${s.parts} fichiers continus réunis">boucle ×${s.parts}</span>` : ""}${s.clips ? `<span class="badge clips">${s.clips} clip${s.clips > 1 ? "s" : ""}</span>` : ""}
        <div class="muted">${s.gps_coverage > 0.05 ? `GPS ${Math.round(s.gps_coverage * 100)} %` : "sans GPS"} · ${esc(folder)}</div></div>`;
    list.appendChild(card);
  });
  if (hidden) list.insertAdjacentHTML("beforeend", `<p class="hint">${hidden} fichier(s) hors projet masqué(s).</p>`);
}
$("#show-all").addEventListener("change", renderFiles);
$("#file-list").addEventListener("click", async (e) => {
  const card = e.target.closest(".file-card");
  if (!card) return;
  if (e.target.closest(".fc-check")) {
    if (e.target.tagName !== "INPUT") return;               // le clic sur le libellé coche la case
    const set = new Set(st.project.sessions);
    e.target.checked ? set.add(card.dataset.sid) : set.delete(card.dataset.sid);
    await api("PUT", "/api/project", { sessions: [...set] });
    return refreshSessions();
  }
  if (!st.s || card.dataset.sid !== st.s.id) await loadSession(card.dataset.sid);
  if (isNarrow()) closeDrawer();
});

// ------------------------------------------------------------------ dossiers de vidéos (menu de l'en-tête)

async function refreshSources() {
  const r = await api("GET", "/api/sources");
  const ul = $("#src-list");
  ul.innerHTML = "";
  r.folders.forEach((f) => {
    const li = document.createElement("li");
    li.innerHTML = `<span class="${f.present ? "muted" : "absent"}" title="${f.present ? "présent" : "absent (carte retirée ?)"}">${f.present ? "●" : "○"}</span>
      <span class="path" title="${f.path}"><bdi dir="ltr">${f.path}</bdi></span>
      <span>${f.sessions} session(s) ${f.removable ? `<button data-rm="${f.path}" title="Retirer ce dossier">✕</button>` : ""}</span>`;
    ul.appendChild(li);
  });
  const sc = r.scan;
  $("#src-status").textContent = sc.state === "running" ? "⏳ " + sc.message : sc.state === "error" ? "⚠ " + sc.message
    : sc.state === "done" ? "✓ " + sc.message : "Tous les sous-dossiers sont parcourus ; les vidéos déjà analysées ne sont pas recalculées.";
  // analyse en cours : on réinterroge, puis on recharge les sessions quand elle est finie
  if (sc.state === "running") setTimeout(async () => { if ((await refreshSources()).scan.state !== "running") refreshSessions(); }, 2000);
  return r;
}
$("#src-menu").addEventListener("toggle", (e) => { if (e.target.open) refreshSources(); });
$("#src-list").addEventListener("click", async (e) => {
  const rm = e.target.dataset.rm;
  if (!rm) return;
  await api("POST", "/api/sources", { remove: rm });
  refreshSources();
});
$("#src-add").addEventListener("submit", async (e) => {
  e.preventDefault();
  const path = $("#src-path").value.trim();
  if (!path) return;
  const r = await apiOrError("POST", "/api/sources", { add: path });
  if (r.error) { $("#src-status").textContent = "⚠ " + r.error; return; }
  $("#src-path").value = "";
  refreshSources();
});
