// Volet « Confidentialité » du montage : analyse des clips (visages, plaques), revue des
// zones détectées (garder / ignorer, supprimer une zone tracée à la main), floutage à l'export.
// Les zones s'affichent aussi sur la vidéo quand le volet est ouvert (zones.js).
// Partage : renderPrivacy.

import { st } from "./state.js";
import { $, api, apiOrError, esc, fmt } from "./util.js";
import { loadPrivacyTracks, setZoneEdit } from "./zones.js";
import { montageKey } from "./montage.js";

let pvTimer = null;
export async function renderPrivacy() {
  clearTimeout(pvTimer);
  const r = await api("GET", "/api/privacy");
  loadPrivacyTracks();
  $("#pv-enabled").checked = r.enabled;
  const j = r.job, running = j.state === "running";
  $("#pv-analyze").disabled = running;
  $("#pv-status").innerHTML = running
    ? `<progress value="${j.progress}" max="1"></progress> ${Math.round(j.progress * 100)} % · ${esc(j.message)} <button id="pv-cancel">annuler</button>`
    : j.state === "done" ? "✓ " + esc(j.message) : j.state === "error" ? "⚠ " + esc(j.message)
    : "Détecte visages et plaques dans chaque clip, dans son cadrage (moteur GPU).";
  if (running) {
    $("#pv-cancel").onclick = () => api("POST", "/api/export/privacy", { cancel: true });
    pvTimer = setTimeout(renderPrivacy, 1500);
  }
  const list = $("#pv-list");
  list.innerHTML = "";
  let kept = 0, pending = 0;
  // numéro du clip dans le montage (clips non exclus)
  const num = new Map(st.project.clips.filter((c) => !c.excluded).map((c, k) => [montageKey(c.sid, c.id), k + 1]));
  const isManual = (t) => String(t.id).startsWith("m");   // zone tracée à la main
  r.clips.forEach((c) => {
    const div = document.createElement("div");
    div.className = "pv-clip";
    const state = !c.analyzed ? "non analysé" : c.stale ? "<span class='pv-stale'>cadrage modifié : à refaire</span>"
      : c.tracks.length ? `${c.tracks.length} zone(s)` : "rien détecté";
    if (!c.analyzed || c.stale) pending++;
    div.innerHTML = `<div class="pv-head"><strong>Clip ${num.get(montageKey(c.sid, c.clip)) ?? "?"}</strong><span class="muted">${state}</span><span class="spacer"></span>
      ${c.tracks.length ? `<button data-all="1">tout garder</button><button data-all="0">tout ignorer</button>` : ""}</div>
      <div class="pv-grid">${c.tracks.map((t) => `<div class="pv-zone${t.enabled ? "" : " off"}${isManual(t) ? " manual" : ""}" data-track="${t.id}" title="${t.kind} · ${isManual(t) ? "tracée à la main" : `confiance ${Math.round(t.conf * 100)} %`} · ${fmt(t.t1 - t.t0 + 0.5)}">
        ${t.thumb ? `<img loading="lazy" alt="" src="/privacy-thumb/${encodeURIComponent(t.thumb)}">` : ""}<span>${t.kind}</span>
        ${isManual(t) ? `<button data-del="${t.id}" title="Supprimer cette zone">✕</button>` : ""}</div>`).join("")}</div>`;
    div.dataset.sid = c.sid; div.dataset.clip = c.clip;
    kept += c.tracks.filter((t) => t.enabled).length;
    list.appendChild(div);
  });
  $("#pv-summary").textContent = `· ${r.enabled ? "flou activé" : "flou désactivé"}` + (kept ? ` · ${kept} zone(s)` : "") + (pending ? ` · ${pending} à analyser` : "");
}

$("#mt-privacy").addEventListener("toggle", (e) => { if (e.target.open) renderPrivacy(); else setZoneEdit(false); });
$("#pv-draw").addEventListener("click", () => setZoneEdit(!st.zoneEdit));
$("#pv-enabled").addEventListener("change", async (e) => {
  await api("PUT", "/api/settings", { privacy: { enabled: e.target.checked } });
  renderPrivacy();
});
$("#pv-analyze").addEventListener("click", async () => {
  const r = await apiOrError("POST", "/api/privacy/analyze", {});
  if (r.error) $("#pv-status").textContent = "⚠ " + r.error;
  renderPrivacy();
});
// vignette : garder ↔ ignorer ; boutons « tout garder / tout ignorer » ; ✕ : supprimer une zone manuelle
$("#pv-list").addEventListener("click", async (e) => {
  const clip = e.target.closest(".pv-clip"), zone = e.target.closest(".pv-zone"), all = e.target.closest("[data-all]");
  if (!clip || (!zone && !all)) return;
  const url = `/api/privacy/${clip.dataset.sid}/${clip.dataset.clip}`;
  if (e.target.dataset.del) {
    await api("PUT", url, { track: e.target.dataset.del, delete: true });
    return renderPrivacy();
  }
  const body = zone ? { track: +zone.dataset.track, enabled: zone.classList.contains("off") }
    : { track: "all", enabled: all.dataset.all === "1" };
  if (zone) zone.classList.toggle("off");
  await api("PUT", url, body);
  renderPrivacy();
});
