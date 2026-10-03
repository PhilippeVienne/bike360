// Dossiers de vidéos (étape ① Fichiers) : liste des dossiers analysés, cartes SD détectées,
// navigateur de dossiers du PC, nouvelle analyse à la demande et suivi de l'analyse en cours
// (le serveur relance aussi seul l'analyse quand de nouveaux fichiers apparaissent).
// Le panneau #src-panel est dessiné ici ; le navigateur est le dialogue #fs.
// Partage : refreshSources.

import { $, api, apiOrError, esc, fmt, storageGet, storageSet } from "./util.js";
import { refreshSessions } from "./project.js";

const panel = $("#src-panel");
const POLL_IDLE_MS = 6000;     // l'analyse automatique du serveur est repérée à ce rythme
const DETECT_EVERY_MS = 30000; // cartes branchées ou retirées

let data = { folders: [], scan: { state: "idle" } };
let detected = [];
let lastDetect = 0;
let timer = null;
let seenFinish = null;         // fin d'analyse déjà prise en compte
const phaseSince = {};         // phase de l'analyse → instant où on l'a vue commencer (ce navigateur)
let notice = "";               // résultat de la dernière action (ajout, erreur…)

// ------------------------------------------------------------------ panneau

function scanHtml(sc) {
  if (sc.state === "running") {
    const total = sc.total || 0, done = Math.min(sc.done || 0, total);
    phaseSince[sc.phase] ??= performance.now();
    const secs = (performance.now() - phaseSince[sc.phase]) / 1000;
    const eta = done >= 2 && secs > 4 && total > done ? ` · reste ~${fmt(secs / done * (total - done))}` : "";
    return `<div class="scan running"><span class="spin"></span><div class="grow">
      <div>${esc(sc.message || "analyse…")}${total ? ` <span class="muted">${done}/${total}${eta}</span>` : ""}</div>
      ${total ? `<progress value="${done}" max="${total}"></progress>` : ""}</div></div>`;
  }
  for (const k of Object.keys(phaseSince)) delete phaseSince[k];
  if (sc.state === "error") return `<div class="scan err">⚠ ${esc(sc.message)}</div>`;
  if (sc.state === "done") return `<div class="scan ok">✓ ${esc(sc.message)}</div>`;
  return "";
}

function render() {
  const sc = data.scan, running = sc.state === "running";
  const cards = detected.filter((d) => !d.added);
  panel.innerHTML = `
    <div class="src-head"><strong>📁 Dossiers de vidéos</strong>
      <span class="muted">${data.folders.length} dossier${data.folders.length > 1 ? "s" : ""}</span>
      <span class="spacer"></span>
      <button data-a="browse" ${running ? "disabled" : ""} title="Choisir un dossier du PC (carte SD, copie sur disque…)">＋ Ajouter un dossier…</button>
      <button data-a="rescan" ${running ? "disabled" : ""} title="Relancer l'analyse des dossiers : nouvelles vidéos, carte rebranchée">↻ Rescanner</button>
    </div>
    ${cards.map((d) => `<div class="src-card">💾 <b>${esc(d.label)}</b> <span class="muted">carte détectée · ${d.sessions} session${d.sessions > 1 ? "s" : ""}</span>
      <span class="spacer"></span><button class="primary" data-add="${esc(d.path)}" ${running ? "disabled" : ""}>Utiliser cette carte</button></div>`).join("")}
    <ul class="src-list">${data.folders.map((f) => `<li>
      <span class="${f.present ? "muted" : "absent"}" title="${f.present ? "présent" : "absent (carte retirée ?)"}">${f.present ? "●" : "○"}</span>
      <span class="path" title="${esc(f.path)}"><bdi dir="ltr">${esc(f.path)}</bdi></span>
      <span>${f.sessions} session${f.sessions > 1 ? "s" : ""}${f.removable ? ` <button data-rm="${esc(f.path)}" ${running ? "disabled" : ""} title="Ne plus analyser ce dossier">✕</button>` : ""}</span></li>`).join("")}</ul>
    ${scanHtml(sc)}
    ${notice ? `<div class="scan err">${esc(notice)}</div>` : ""}
    <span class="hint">Les sous-dossiers sont parcourus. Les nouvelles vidéos sont repérées automatiquement ; les vidéos déjà analysées ne sont pas recalculées.</span>`;
}

// ------------------------------------------------------------------ suivi

/** Relit l'état du serveur ; recharge les sessions quand une analyse vient de se terminer. */
export async function refreshSources() {
  clearTimeout(timer);
  try {
    data = await api("GET", "/api/sources");
    if (performance.now() - lastDetect > DETECT_EVERY_MS || !lastDetect) {
      lastDetect = performance.now() || 1;
      detected = await api("GET", "/api/sources/detect").catch(() => detected);
    }
  } catch (e) { /* serveur injoignable : on réessaie plus tard */ }
  const sc = data.scan;
  if (sc.state !== "running" && sc.finished && sc.finished !== seenFinish) {
    const first = seenFinish === null;
    seenFinish = sc.finished;
    if (!first) { await refreshSessions(); detected = await api("GET", "/api/sources/detect").catch(() => detected); }
  }
  render();
  timer = setTimeout(refreshSources, sc.state === "running" ? 1000 : document.hidden ? POLL_IDLE_MS * 3 : POLL_IDLE_MS);
  return data;
}

async function post(body) {
  notice = "";
  const r = await apiOrError("POST", "/api/sources", body);
  if (r.error) { notice = "⚠ " + r.error; render(); return r; }
  lastDetect = 0;
  await refreshSources();
  return r;
}

panel.addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b) return;
  if (b.dataset.a === "browse") return openBrowser();
  if (b.dataset.a === "rescan") return post({ rescan: true });
  if (b.dataset.rm) return post({ remove: b.dataset.rm });
  if (b.dataset.add) return post({ add: b.dataset.add });
});

// ------------------------------------------------------------------ navigateur de dossiers

const fs = $("#fs");
let here = null;   // réponse de /api/fs pour le dossier affiché

async function browse(path) {
  const r = await apiOrError("GET", `/api/fs?path=${encodeURIComponent(path)}`);
  if (r.error) { $("#fs-status").textContent = "⚠ " + r.error; return; }
  here = r;
  storageSet("bike360.fs", r.path);
  $("#fs-path").value = r.path;
  $("#fs-up").disabled = !r.parent;
  $("#fs-shortcuts").innerHTML = r.shortcuts
    .map((s) => `<button data-go="${esc(s.path)}">${esc(s.label)}</button>`).join("");
  $("#fs-list").innerHTML = r.dirs.length ? r.dirs.map((d) => `<li data-go="${esc(d.path)}">
      <span class="ico">📁</span><span class="nm">${esc(d.name)}</span>
      ${d.videos ? `<span class="badge">${d.videos} vidéo${d.videos > 1 ? "s" : ""}</span>` : ""}
      ${d.videos ? `<button class="primary" data-pick="${esc(d.path)}">Ajouter</button>` : ""}</li>`).join("")
    : `<li class="hint">Aucun sous-dossier.</li>`;
  $("#fs-status").textContent = r.videos_here ? `${r.videos_here} fichier(s) de la caméra dans ce dossier.` : "Aucune vidéo de la caméra directement ici (les sous-dossiers sont parcourus à l'analyse).";
  $("#fs-pick").disabled = false;
  $("#fs-list").scrollTop = 0;
}

function openBrowser() {
  fs.showModal();
  browse(storageGet("bike360.fs") || detected[0]?.path || "");
}

async function pick(path) {
  $("#fs-status").textContent = "Vérification…";
  const r = await apiOrError("POST", "/api/sources", { add: path });
  if (r.error) { $("#fs-status").textContent = "⚠ " + r.error; return; }
  fs.close();
  notice = "";
  lastDetect = 0;
  refreshSources();
}

fs.addEventListener("click", (e) => {
  if (e.target === fs || e.target.closest("[data-close]")) return fs.close();
  const p = e.target.closest("[data-pick]");
  if (p) return pick(p.dataset.pick);
  const g = e.target.closest("[data-go]");
  if (g) return browse(g.dataset.go);
  if (e.target.closest("#fs-up") && here?.parent) return browse(here.parent);
  if (e.target.closest("#fs-pick") && here) return pick(here.path);
});
$("#fs-form").addEventListener("submit", (e) => { e.preventDefault(); browse($("#fs-path").value.trim()); });

refreshSources();
