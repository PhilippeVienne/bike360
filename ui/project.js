// Fichiers du projet (étape ①) : sessions groupées par jour, case « dans le projet » pour
// les inclure au montage, ouverture d'une session ; sélecteur de session de l'étape ②.
// (Les dossiers de vidéos sont gérés par sources.js.)
// Partage : refreshSessions.

import { st } from "./state.js";
import { $, api, dayLabel, esc, fmt, hhmm } from "./util.js";
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
        <div class="muted">${s.gps_coverage > 0.05 ? `GPS ${Math.round(s.gps_coverage * 100)} %${s.gps_source === "gpx" ? " (trace .gpx)" : ""}` : "sans GPS"}${s.camera ? ` · ${esc(s.camera.model)}` : ""} · ${esc(folder)}</div></div>`;
    list.appendChild(card);
  });
  if (!st.sessions.length) list.innerHTML = `<div class="empty">Aucune vidéo analysée pour l'instant.<br>
    Branche ta carte SD ou ajoute un dossier dans « Dossiers de vidéos » ci-dessus.</div>`;
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
