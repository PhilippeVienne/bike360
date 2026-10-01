// Fichiers du projet (étape ①) : sessions groupées par jour, case « dans le projet » pour
// les inclure au montage, ouverture d'une session ; sélecteur de session de l'étape ② ;
// dossiers de vidéos analysés (Réglages).
// Partage : refreshSessions, refreshSources.

import { st } from "./state.js";
import { $, api, apiOrError, dayLabel, esc, fmt, hhmm } from "./util.js";
import { loadSession } from "./session.js";
import { renderMontage } from "./montage.js";
import { goStep, updateSteps } from "./steps.js";

/** Recharge la liste des sessions et le projet (clips du montage), puis fichiers et montage. */
export async function refreshSessions() {
  [st.sessions, st.project] = await Promise.all([api("GET", "/api/sessions"), api("GET", "/api/project")]);
  renderFiles();
  renderSessionPick();
  renderMontage();
  updateSteps();
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
      <label class="fc-check" title="Inclure ce fichier dans le projet (montage)"><input type="checkbox" ${inside ? "checked" : ""}> dans le projet</label>
      <div class="fc-info"><strong>${hhmm(s)}</strong> · ${fmt(s.duration)}${s.parts > 1 ? `<span class="badge" title="Enregistrement en boucle : ${s.parts} fichiers continus réunis">boucle ×${s.parts}</span>` : ""}${s.clips ? `<span class="badge clips">${s.clips} clip${s.clips > 1 ? "s" : ""}</span>` : ""}
        <div class="muted">${s.gps_coverage > 0.05 ? `GPS ${Math.round(s.gps_coverage * 100)} %` : "sans GPS"} · ${esc(folder)}</div></div>`;
    list.appendChild(card);
  });
  if (!st.sessions.length) list.innerHTML = `<div class="empty">Aucune vidéo analysée pour l'instant.<br>
    Ajoute le dossier de ta carte SD (ou d'une copie) dans <button data-open="settings">⚙ Réglages → Dossiers de vidéos</button>.</div>`;
  else if (!inProj.size && !all) list.innerHTML = `<div class="empty">Aucune session dans le projet.<br>
    Coche « afficher aussi les fichiers hors projet » ci-dessus, puis « dans le projet » sur les sessions de ta balade.</div>`;
  if (hidden && inProj.size) list.insertAdjacentHTML("beforeend", `<p class="hint">${hidden} fichier(s) hors projet masqué(s).</p>`);
}

/** Sélecteur de la session affichée (étape ②) : sessions du projet, plus la session affichée. */
function renderSessionPick() {
  const sel = $("#session-pick"), inProj = new Set(st.project.sessions);
  const shown = st.sessions.filter((s) => inProj.has(s.id) || (st.s && st.s.id === s.id));
  let html = "", day = null;
  for (const s of shown) {
    if (s.date !== day) { html += (day ? "</optgroup>" : "") + `<optgroup label="${dayLabel(s.date)}">`; day = s.date; }
    html += `<option value="${s.id}">${hhmm(s)} · ${fmt(s.duration)}${s.clips ? ` · ${s.clips} clip${s.clips > 1 ? "s" : ""}` : ""}${inProj.has(s.id) ? "" : " (hors projet)"}</option>`;
  }
  sel.innerHTML = html + (day ? "</optgroup>" : "");
  if (st.s) sel.value = st.s.id;
}
$("#session-pick").addEventListener("change", (e) => loadSession(e.target.value));

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
  goStep("cut");
});

// ------------------------------------------------------------------ dossiers de vidéos (Réglages)

export async function refreshSources() {
  const r = await api("GET", "/api/sources");
  const ul = $("#src-list");
  ul.innerHTML = "";
  r.folders.forEach((f) => {
    const li = document.createElement("li");
    li.innerHTML = `<span class="${f.present ? "muted" : "absent"}" title="${f.present ? "présent" : "absent (carte retirée ?)"}">${f.present ? "●" : "○"}</span>
      <span class="path" title="${esc(f.path)}"><bdi dir="ltr">${esc(f.path)}</bdi></span>
      <span>${f.sessions} session(s) ${f.removable ? `<button data-rm="${esc(f.path)}" title="Retirer ce dossier">✕</button>` : ""}</span>`;
    ul.appendChild(li);
  });
  showFolderCount(r.folders.length);
  const sc = r.scan;
  $("#src-status").textContent = sc.state === "running" ? "⏳ " + sc.message : sc.state === "error" ? "⚠ " + sc.message
    : sc.state === "done" ? "✓ " + sc.message : "Tous les sous-dossiers sont parcourus ; les vidéos déjà analysées ne sont pas recalculées.";
  // analyse en cours : on réinterroge, puis on recharge les sessions quand elle est finie
  if (sc.state === "running") setTimeout(async () => { if ((await refreshSources()).scan.state !== "running") refreshSessions(); }, 2000);
  return r;
}
/** Nombre de dossiers, rappelé sur la page Fichiers. */
function showFolderCount(n) {
  const el = $("#src-count");
  if (el) el.textContent = `${n} dossier${n > 1 ? "s" : ""} de vidéos`;
}
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
$("#settings").addEventListener("open-dialog", refreshSources);
refreshSources();
