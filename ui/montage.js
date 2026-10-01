// Étape ③ Montage : tous les clips du projet dans un ordre libre (glisser ou ↑↓), avec
// exclusions ; montage automatique ; aperçu de la mini-carte incrustée. Export du montage
// (étape ④) et suivi de sa progression (annulation, lien du fichier, chapitres YouTube).
// Partage : montageKey, renderMontage, updateMontageMap, pollMontage.

import { st } from "./state.js";
import { $, $$, api, apiOrError, esc, exportLink, fmt } from "./util.js";
import { renderClips, saveClipsNow, selectClip } from "./clips.js";
import { drawClipsOnMap } from "./map.js";
import { loadSession } from "./session.js";
import { refreshSessions } from "./project.js";
import { exportSettings } from "./export.js";
import { renderFinish } from "./finish.js";
import { isStep, onStep, onTab, setExported, setJob, tabShown, updateSteps } from "./steps.js";

/** Identifiant d'un clip dans le projet (les id de clip ne sont uniques que par session). */
export const montageKey = (sid, clipId) => `${sid}|${clipId}`;
const itemKey = (c) => montageKey(c.sid, c.id);

export function renderMontage() {
  const ul = $("#mt-list"), clips = st.project.clips;
  ul.innerHTML = clips.length ? "" : `<li class="empty">Aucun clip dans les sessions du projet.<br>
    Repère des moments à l'étape <button data-go="cut">② Repérer &amp; couper</button>, ou laisse faire ✨ le montage automatique ci-dessus.</li>`;
  let n = 0;
  clips.forEach((c) => {
    const li = document.createElement("li");
    li.className = "mt-item" + (c.excluded ? " excluded" : "") + (st.s && st.s.id === c.sid && st.clips[st.sel]?.id === c.id ? " current" : "");
    li.draggable = true;
    li.dataset.key = itemKey(c);
    const when = new Date(c.utc * 1000).toLocaleString("fr-FR", { day: "2-digit", month: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit" });
    const t = (c.start + Math.min(2, (c.end - c.start) / 2)).toFixed(1);   // vignette prise 2 s après le début
    li.innerHTML = `<span class="grip">⋮⋮</span>
      <img loading="lazy" alt="" src="/thumb/${c.sid}.jpg?t=${t}&yaw=${c.yaw}&pitch=${c.pitch}&fov=${c.fov}">
      <div><span class="n">${c.excluded ? "–" : ++n}</span>${when}<div class="muted">${fmt(c.end - c.start)}${c.auto ? ' <span class="badge auto">auto</span>' : ""}</div></div>
      <div class="mt-btns"><button data-mv="-1" title="Monter">↑</button><button data-ex title="${c.excluded ? "Inclure" : "Exclure"} du montage">${c.excluded ? "◌" : "👁"}</button><button data-mv="1" title="Descendre">↓</button></div>`;
    ul.appendChild(li);
  });
  const kept = clips.filter((c) => !c.excluded);
  const total = kept.reduce((a, c) => a + c.end - c.start, 0);
  $("#mt-total").textContent = kept.length ? `${kept.length} clip(s) · ${fmt(total)}` + (clips.length > kept.length ? ` · ${clips.length - kept.length} exclu(s)` : "") : "";
  $("#ex-montage-sum").textContent = kept.length ? $("#mt-total").textContent : "aucun clip : voir l'étape ③";
  $("#mt-start").disabled = $("#mt-preview").disabled = !kept.length;
  updateMontageMap();
  if (isStep("montage") || isStep("export")) renderFinish();
  updateSteps();
}

/** Aperçu de la mini-carte d'export : clip sélectionné (ou premier du montage). */
export function updateMontageMap() {
  if (!tabShown("mt", "minimap")) return;
  const sel = st.s && st.clips[st.sel];
  const c = (sel && st.project.clips.find((x) => x.sid === st.s.id && x.id === sel.id))
    || st.project.clips.find((x) => !x.excluded);
  const img = $("#mt-map");
  img.parentElement.hidden = !c;
  if (!c) return;
  const size = Math.round(Math.min(640, img.clientWidth * devicePixelRatio || 320));
  const src = `/minimap.png?sid=${c.sid}&clip=${c.id}&size=${size}&v=${c.start}-${c.end}-${st.project.clips.length}`;
  if (img.getAttribute("src") !== src) img.src = src;
}

// ------------------------------------------------------------------ ordre et exclusions

async function saveMontage(clips) {
  st.project.clips = clips;
  renderMontage();
  await api("PUT", "/api/project", { order: clips.map((c) => [c.sid, c.id]),
                                     excluded: clips.filter((c) => c.excluded).map((c) => [c.sid, c.id]) });
}
const list = $("#mt-list");
list.addEventListener("click", async (e) => {
  const li = e.target.closest(".mt-item");
  if (!li) return;
  const clips = [...st.project.clips], i = clips.findIndex((c) => itemKey(c) === li.dataset.key);
  const b = e.target.closest("button");
  if (b && b.dataset.mv) {
    const j = i + +b.dataset.mv;
    if (j < 0 || j >= clips.length) return;
    [clips[i], clips[j]] = [clips[j], clips[i]];
    return saveMontage(clips);
  }
  if (b && "ex" in b.dataset) { clips[i] = { ...clips[i], excluded: !clips[i].excluded }; return saveMontage(clips); }
  // clic sur le clip : l'ouvrir dans sa session
  const c = clips[i];
  if (!st.s || st.s.id !== c.sid) await loadSession(c.sid);
  const k = st.clips.findIndex((x) => x.id === c.id);
  if (k >= 0) selectClip(k, "seek");
  renderMontage();
});
// glisser-déposer : un trait avant ou après la ligne survolée montre l'emplacement
let dragKey = null;
list.addEventListener("dragstart", (e) => {
  const li = e.target.closest(".mt-item");
  if (!li) return;
  dragKey = li.dataset.key;
  li.classList.add("dragging");
  e.dataTransfer.effectAllowed = "move";
  e.dataTransfer.setData("text/plain", dragKey);
});
list.addEventListener("dragover", (e) => {
  const li = e.target.closest(".mt-item");
  if (!li || !dragKey) return;
  e.preventDefault();
  const after = e.clientY > li.getBoundingClientRect().top + li.offsetHeight / 2;
  $$(".drop-before, .drop-after").forEach((x) => x.classList.remove("drop-before", "drop-after"));
  li.classList.add(after ? "drop-after" : "drop-before");
});
list.addEventListener("drop", (e) => {
  e.preventDefault();
  const li = e.target.closest(".mt-item");
  if (!li || !dragKey || li.dataset.key === dragKey) return;
  const after = li.classList.contains("drop-after");
  const clips = st.project.clips.filter((c) => itemKey(c) !== dragKey);
  const moved = st.project.clips.find((c) => itemKey(c) === dragKey);
  clips.splice(clips.findIndex((c) => itemKey(c) === li.dataset.key) + (after ? 1 : 0), 0, moved);
  saveMontage(clips);
});
list.addEventListener("dragend", () => {
  dragKey = null;
  $$(".dragging, .drop-before, .drop-after").forEach((x) => x.classList.remove("dragging", "drop-before", "drop-after"));
});

// ------------------------------------------------------------------ montage automatique : meilleurs moments du jour affiché (ou du projet), clips marqués « auto »

async function autoMontage(clear) {
  const scope = $("#auto-scope").value;
  const sids = scope === "day" && st.s ? st.sessions.filter((x) => x.date === st.s.date).map((x) => x.id) : null;
  $("#mt-total").textContent = clear ? "Retrait des clips auto…" : "Recherche des meilleurs moments…";
  const r = await api("POST", "/api/automontage", { duration: +$("#auto-duration").value, sids, clear });
  if (st.s) {   // la session affichée a pu changer : on recharge ses clips
    const s2 = await api("GET", `/api/session/${st.s.id}`);
    st.clips = s2.selections || [];
    st.sel = null;
    renderClips(); drawClipsOnMap();
  }
  await refreshSessions();
  $("#mt-total").textContent = clear ? `${r.removed} clip(s) auto retiré(s)`
    : `✨ ${r.added} clip(s) ajouté(s) (${fmt(r.seconds)})` + (r.removed ? `, ${r.removed} ancien(s) remplacé(s)` : "");
}
$("#auto-make").addEventListener("click", () => autoMontage(false));
$("#auto-clear").addEventListener("click", () => autoMontage(true));

// ------------------------------------------------------------------ export du montage

async function startMontage(quality) {
  await saveClipsNow();   // la session affichée est à jour
  const r = await apiOrError("POST", "/api/montage", { quality, ...exportSettings() });
  if (r.error) { $("#mt-status").textContent = "⚠ " + r.error; return; }
  pollMontage();
}
$("#mt-start").addEventListener("click", () => startMontage("final"));
$("#mt-preview").addEventListener("click", () => startMontage("preview"));

let montageTimer = null;
export async function pollMontage() {
  clearTimeout(montageTimer);
  const j = await api("GET", "/api/export/montage");
  const el = $("#mt-status");
  if (j.state === "running") {
    el.innerHTML = `<progress value="${j.progress}" max="1"></progress> ${Math.round(j.progress * 100)} % · ${j.message}
      <button id="mt-cancel">annuler</button>`;
    $("#mt-cancel").onclick = () => api("POST", "/api/export/montage", { cancel: true });
    montageTimer = setTimeout(pollMontage, 1000);
    setJob("montage", `Montage ${Math.round(j.progress * 100)} %`);
  } else {
    setJob("montage", null);
    if (j.state === "done") setExported(j.output);
    el.innerHTML = j.state === "done" ? exportLink(j.output)
      : j.state === "error" ? "⚠ " + j.message.slice(0, 200) : "";
    if (j.warning) el.insertAdjacentHTML("beforeend", ` <span class="warn">⚠ ${j.warning}</span>`);
    if (j.state === "done" && j.chapters) {   // chapitres YouTube : à coller dans la description
      el.insertAdjacentHTML("beforeend", `<div class="hint">Chapitres YouTube (description de la vidéo) :
        <button id="mt-copy-chapters">copier</button></div><textarea id="mt-chapters" readonly rows="${Math.min(8, j.chapters.split("\n").length)}">${esc(j.chapters)}</textarea>`);
      $("#mt-copy-chapters").onclick = () => { $("#mt-chapters").select(); (navigator.clipboard ? navigator.clipboard.writeText(j.chapters) : Promise.reject()).catch(() => document.execCommand("copy")); };
    }
  }
}

// entrée dans l'étape ③ : projet relu (clips des autres sessions), état de l'export
onStep((step) => { if (step === "montage") { refreshSessions(); pollMontage(); } });
onTab((g, name) => { if (name === "minimap") updateMontageMap(); });
