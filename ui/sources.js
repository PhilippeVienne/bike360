// Dossiers de vidéos (étape ① Fichiers) : liste des dossiers analysés, cartes SD détectées,
// navigateur de dossiers du PC, nouvelle analyse à la demande et suivi de l'analyse en cours
// (le serveur relance aussi seul l'analyse quand de nouveaux fichiers apparaissent).
// Les traces GPS déposées (.gpx) sont listées au même endroit : elles remplacent GeoRide ces jours-là.
// Le panneau #src-panel est dessiné ici ; le navigateur est le dialogue #fs (à trois panneaux).
// Partage : refreshSources.

import { $, api, apiOrError, esc, fmt, storageGet, storageSet } from "./util.js";
import { refreshSessions } from "./project.js";

const panel = $("#src-panel");
const POLL_IDLE_MS = 6000;     // l'analyse automatique du serveur est repérée à ce rythme
const DETECT_EVERY_MS = 30000; // cartes branchées ou retirées

let data = { folders: [], scan: { state: "idle" } };
let detected = [];
let gps = [];                  // traces GPX déposées : [{name, points, from, to}]
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
    <div class="src-head"><strong>🛰 Traces GPS</strong>
      <span class="muted">${gps.length ? `${gps.length} fichier${gps.length > 1 ? "s" : ""}` : "aucune : positions GeoRide si un compte est configuré"}</span>
      <span class="spacer"></span>
      <label class="upload" title="Déposer une trace .gpx (téléphone, GPS, traceur) : elle sert de source de positions pour les sessions de ces jours-là">＋ Trace .gpx<input type="file" class="gps-upload" accept=".gpx" hidden ${running ? "disabled" : ""}></label>
    </div>
    <ul class="src-list">${gps.map((g) => `<li>
      <span class="muted">●</span>
      <span class="path" title="${esc(g.name)}"><bdi dir="ltr">${esc(g.name)}</bdi></span>
      <span>${g.from ? `${new Date(g.from).toLocaleDateString("fr-FR", { day: "numeric", month: "short", year: "numeric" })} · ` : ""}${g.points} points
        <button data-gps-rm="${esc(g.name)}" ${running ? "disabled" : ""} title="Retirer cette trace">✕</button></span></li>`).join("")}</ul>
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
      gps = await api("GET", "/api/gps").catch(() => gps);
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
  if (b.dataset.gpsRm) return postGps(`remove=${encodeURIComponent(b.dataset.gpsRm)}`);
});

/** Dépose (corps = fichier) ou retire une trace GPX, puis suit la nouvelle analyse. */
async function postGps(query, file) {
  notice = "";
  const r = await fetch(`/api/gps?${query}`, { method: "POST", body: file }).then((x) => x.json()).catch(() => ({ error: "serveur injoignable" }));
  if (r.error) { notice = "⚠ " + r.error; render(); return; }
  gps = r.files;
  await refreshSources();
}

panel.addEventListener("change", (e) => {
  const f = e.target.matches(".gps-upload") && e.target.files[0];
  if (f) postGps(`name=${encodeURIComponent(f.name)}`, f);
});

// ------------------------------------------------------------------ navigateur de dossiers
// Emplacements et cartes à gauche, dossiers au centre (fil d'Ariane, filtre), aperçu des
// sessions trouvées à droite avant d'ajouter.

const fs = $("#fs");
let here = null;           // réponse de /api/fs pour le dossier affiché
let preview = null;        // réponse de /api/fs/preview ("loading" pendant le calcul)
let browseToken = 0;       // ignore les réponses d'un dossier qu'on a déjà quitté

const sizeText = (b) => b >= 1e9 ? (b / 1e9).toFixed(1).replace(".", ",") + " Go" : Math.max(1, Math.round(b / 1e6)) + " Mo";
const sessionsText = (n) => `${n} session${n > 1 ? "s" : ""}`;

async function browse(path) {
  const token = ++browseToken;
  const r = await apiOrError("GET", `/api/fs?path=${encodeURIComponent(path)}`);
  if (token !== browseToken) return;
  if (r.error) { $("#fs-status").textContent = "⚠ " + r.error; return; }
  here = r;
  preview = "loading";
  storageSet("bike360.fs", r.path);
  $("#fs-filter").value = "";
  $("#fs-form").hidden = true;
  $("#fs-crumbs").hidden = false;
  renderBrowser();
  const p = await apiOrError("GET", `/api/fs/preview?path=${encodeURIComponent(r.path)}`);
  if (token !== browseToken) return;
  preview = p.error ? { error: p.error } : p;
  renderPreview();
}

function renderBrowser() {
  const r = here;
  $("#fs-up").disabled = !r.parent;
  // fil d'Ariane : chaque segment ramène à ce dossier
  const parts = r.path.split("/").filter(Boolean);
  $("#fs-crumbs").innerHTML = `<button data-dir="/">/</button>` + parts.map((p, i) =>
    `<button data-dir="/${esc(parts.slice(0, i + 1).join("/"))}"${i === parts.length - 1 ? ' class="active"' : ""}>${esc(p)}</button>`).join("");
  $("#fs-crumbs").scrollLeft = 1e6;
  // barre latérale : cartes détectées, emplacements, dossiers déjà analysés
  const added = new Set(data.folders.map((f) => f.path));
  $("#fs-side").innerHTML = `
    ${detected.length ? `<h4>Cartes SD</h4>${detected.map((d) => `<div class="fs-card">
      <button data-dir="${esc(d.path)}" title="${esc(d.path)}">💾 <b>${esc(d.label)}</b><small>${sessionsText(d.sessions)}</small></button>
      ${added.has(d.path) ? `<span class="ok">✓ ajoutée</span>` : `<button class="primary" data-pick="${esc(d.path)}">Utiliser</button>`}</div>`).join("")}` : ""}
    <h4>Emplacements</h4>${r.shortcuts.map((s) => `<button data-dir="${esc(s.path)}" class="${s.path === r.path ? "active" : ""}">${esc(s.label)}</button>`).join("")}
    ${data.folders.length ? `<h4>Déjà analysés</h4>${data.folders.map((f) => `<button data-dir="${esc(f.path)}" title="${esc(f.path)}" class="${f.present ? "" : "absent"}">${esc(f.path.split("/").filter(Boolean).pop() || f.path)}</button>`).join("")}` : ""}`;
  renderList();
  renderPreview();
}

function renderList() {
  const q = $("#fs-filter").value.trim().toLowerCase();
  // dossiers qui contiennent des vidéos de la caméra d'abord
  const dirs = here.dirs.filter((d) => !q || d.name.toLowerCase().includes(q))
    .sort((a, b) => (!!b.videos - !!a.videos) || a.name.localeCompare(b.name, "fr", { numeric: true }));
  $("#fs-list").innerHTML = dirs.length ? dirs.map((d) => `<li data-dir="${esc(d.path)}" class="${d.videos ? "has" : ""}">
      <span class="ico">${d.videos ? "🎞" : "📁"}</span><span class="nm">${esc(d.name)}</span>
      ${d.videos ? `<span class="badge">${d.videos} fichier${d.videos > 1 ? "s" : ""}</span>` : ""}<span class="chev">›</span></li>`).join("")
    : `<li class="hint empty">${q ? "Aucun dossier ne correspond." : "Aucun sous-dossier."}</li>`;
}

function renderPreview() {
  const box = $("#fs-prev"), pick = $("#fs-pick"), st = $("#fs-status");
  if (!here) return;
  const name = here.path.split("/").filter(Boolean).pop() || "/";
  if (preview === "loading") {
    box.innerHTML = `<h4>${esc(name)}</h4><div class="scan"><span class="spin"></span> Recherche des vidéos…</div>`;
    pick.disabled = true; pick.textContent = "Ajouter ce dossier"; st.textContent = "";
    return;
  }
  if (preview.error) { box.innerHTML = `<h4>${esc(name)}</h4><div class="scan err">⚠ ${esc(preview.error)}</div>`; pick.disabled = true; return; }
  const n = preview.total, isAdded = data.folders.some((f) => f.path === here.path);
  box.innerHTML = `<h4>${esc(name)}</h4>` + (n
    ? `<p class="hint">${sessionsText(n)} de la caméra dans ce dossier et ses sous-dossiers${preview.truncated ? " (exploration partielle : dossier très grand)" : ""}.</p>
       <ul class="fs-sessions">${preview.sessions.map((s) => `<li class="${s.known ? "known" : ""}">
         <b>${new Date(`${s.date.slice(0, 4)}-${s.date.slice(4, 6)}-${s.date.slice(6)}T12:00`).toLocaleDateString("fr-FR", { weekday: "short", day: "numeric", month: "short", year: "numeric" })}</b>
         · ${s.time.slice(0, 2)}h${s.time.slice(2, 4)}<small>${s.files} fichier${s.files > 1 ? "s" : ""} · ${sizeText(s.bytes)}${s.known ? " · déjà analysée" : ""}</small></li>`).join("")}</ul>`
    : `<p class="hint">Aucune vidéo de la caméra (.insv / .lrv) ici. Ouvre un sous-dossier : ceux qui en contiennent sont en tête de liste (🎞).</p>`);
  pick.disabled = !n || isAdded;
  pick.textContent = isAdded ? "✓ Déjà ajouté" : n ? `Ajouter ce dossier (${sessionsText(n)})` : "Ajouter ce dossier";
  st.textContent = "";
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
  const g = e.target.closest("[data-dir]");
  if (g) return browse(g.dataset.dir);
  if (e.target.closest("#fs-up") && here?.parent) return browse(here.parent);
  if (e.target.closest("#fs-pick") && here) return pick(here.path);
  if (e.target.closest("#fs-edit")) {   // saisie directe d'un chemin
    const f = $("#fs-form");
    f.hidden = !f.hidden;
    $("#fs-crumbs").hidden = !f.hidden;
    if (!f.hidden) { $("#fs-path").value = here?.path || ""; $("#fs-path").select(); }
  }
});
$("#fs-form").addEventListener("submit", (e) => { e.preventDefault(); browse($("#fs-path").value.trim()); });
$("#fs-filter").addEventListener("input", () => here && renderList());

refreshSources();
