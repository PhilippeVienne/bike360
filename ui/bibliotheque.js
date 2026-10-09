// Bibliothèque (module séparé de l'atelier) : tous les rushs du compte, par balade, avec la place
// occupée, les marqueurs (favori, garder, corbeille), l'allègement et les suggestions de nettoyage.

import { authFetch, showAccount } from "./compte-session.js";

const $ = (s) => document.querySelector(s);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const size = (b) => b >= 1e11 ? `${Math.round(b / 1e9)} Go` : b >= 1e9 ? `${(b / 1e9).toFixed(1).replace(".", ",")} Go` : `${Math.round(b / 1e6)} Mo`;
const dur = (s) => { s = Math.round(s); return s >= 3600 ? `${Math.floor(s / 3600)} h ${String(Math.floor(s % 3600 / 60)).padStart(2, "0")}` : s >= 60 ? `${Math.floor(s / 60)} min` : `${s} s`; };
const day = (d) => new Date(`${d.slice(0, 4)}-${d.slice(4, 6)}-${d.slice(6)}T12:00`).toLocaleDateString("fr-FR", { weekday: "long", day: "numeric", month: "long", year: "numeric" });
const CLASSES = { INTELLIGENT_TIERING: "accès immédiat", GLACIER_IR: "accès immédiat", DEEP_ARCHIVE: "archivé (retour sous 48 h)", STANDARD: "accès immédiat" };
const MARKS = [["favori", "★", "Favori"], ["garder", "✓", "À garder"], ["corbeille", "🗑", "Mettre à la corbeille"]];

let lib = null;

async function call(method, path, body) {
  const r = await authFetch(`/api/bibliotheque${path}`, { method, headers: { "Content-Type": "application/json" },
                                                     body: body === undefined ? undefined : JSON.stringify(body) });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || `${r.status}`);
  return j;
}

function sessionRow(s) {
  const o = s.originals, p = s.previews;
  const state = s.mark === "corbeille" ? `<span class="badge">corbeille · supprimée dans ${s.trash_days_left} j</span>`
    : !o.files ? `<span class="badge">aperçu seul</span>` : "";
  const facts = !s.analysed ? "analyse en attente"
    : [s.distance_km != null ? `${String(s.distance_km).replace(".", ",")} km` : s.gps_coverage < 0.05 ? "sans GPS" : "",
       s.candidates ? `${s.candidates} moment${s.candidates > 1 ? "s" : ""} fort${s.candidates > 1 ? "s" : ""}` : ""].filter(Boolean).join(" · ");
  return `<li class="lib-row${s.mark === "corbeille" ? " trashed" : ""}" data-sid="${esc(s.id)}">
    ${s.thumb ? `<img class="thumb" loading="lazy" alt="" src="${esc(s.thumb)}">` : `<span class="thumb"></span>`}
    <span class="when"><strong>${s.time.slice(0, 2)}h${s.time.slice(2, 4)}</strong> · ${dur(s.duration_s)}</span>
    <span class="muted grow">${facts ? `${facts} · ` : ""}${esc(s.camera || "caméra inconnue")}${s.angles.length ? ` · ${s.angles.length + 1} angles` : ""}
      · aperçu ${size(p.bytes)}${o.files ? ` · originaux ${size(o.bytes)}, ${CLASSES[o.class] || esc(o.class)}` : ""} ${state}</span>
    <span class="marks">${MARKS.map(([key, icon, title]) =>
      `<button data-mark="${key}" class="${s.mark === key ? "on" : ""}" title="${title}">${icon}</button>`).join("")}
      <button data-lighten ${o.files ? "" : "disabled"} title="Supprimer les originaux et garder l'aperçu : libère ${size(o.bytes)}">Alléger</button></span></li>`;
}

function render() {
  const used = lib.bytes / lib.quota_bytes;
  $("#gauge").innerHTML = `<div class="row"><strong>${size(lib.bytes)}</strong> <span class="muted">sur ${size(lib.quota_bytes)}</span></div>
    <progress value="${Math.min(1, used)}" max="1"></progress>`;
  $("#suggestions").innerHTML = lib.suggestions.length
    ? `<h3>Suggestions de nettoyage</h3><ul class="lib-list">${lib.suggestions.map((g) => `<li class="lib-row" data-sid="${esc(g.session)}">
        <span class="grow"><strong>${g.session.slice(10, 12)}/${g.session.slice(8, 10)} à ${g.session.slice(13, 15)}h${g.session.slice(15, 17)}</strong> · ${esc(g.text)} <span class="muted">${size(g.bytes)}</span></span>
        <span class="marks"><button data-mark="corbeille">Mettre à la corbeille</button><button data-mark="garder">Garder</button></span></li>`).join("")}</ul>` : "";
  $("#rides").innerHTML = lib.rides.length
    ? lib.rides.map((r) => `<section class="ride"><div class="day-head">${day(r.date)} · ${r.sessions.length} session${r.sessions.length > 1 ? "s" : ""} · ${size(r.bytes)}</div>
        <ul class="lib-list">${r.sessions.map(sessionRow).join("")}</ul></section>`).join("")
    : `<p class="muted">Aucun rush pour l'instant. <a href="envoi.html">Envoyer mes rushs</a></p>`;
  const trashed = lib.rides.flatMap((r) => r.sessions).filter((s) => s.mark === "corbeille").length;
  $("#trash").textContent = trashed ? `${trashed} session${trashed > 1 ? "s" : ""} à la corbeille, gardée${trashed > 1 ? "s" : ""} ${lib.trash_days} jours avant suppression.` : "";
}

async function refresh() {
  try { lib = await call("GET", ""); $("#error").textContent = ""; render(); }
  catch (e) { $("#error").textContent = `⚠ ${e.message}`; }
}

document.addEventListener("click", async (e) => {
  const b = e.target.closest("button"), row = e.target.closest("[data-sid]");
  if (!b || !row) return;
  const sid = row.dataset.sid;
  try {
    if (b.dataset.mark) {
      const current = lib.rides.flatMap((r) => r.sessions).find((s) => s.id === sid);
      await call("POST", "/marque", { session: sid, mark: current && current.mark === b.dataset.mark ? null : b.dataset.mark });
    } else if ("lighten" in b.dataset) {
      // suppression définitive : on demande confirmation, avec ce qui sera perdu
      if (!confirm("Supprimer définitivement les originaux de cette session ? L'aperçu et les clips sont gardés, mais l'export en pleine qualité ne sera plus possible.")) return;
      await call("POST", "/alleger", { session: sid, confirm: true });
    }
  } catch (err) { $("#error").textContent = `⚠ ${err.message}`; return; }
  refresh();
});
showAccount($("#account"));
refresh();
