// Page du palier : place et minutes d'export du compte, grille des paliers, passage au paiement.
import { authFetch, showAccount } from "./compte-session.js";

const $ = (s) => document.querySelector(s);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const go = (b) => `${(b / 1e9).toFixed(b >= 1e11 ? 0 : 1).replace(".", ",")} Go`;
const min = (s) => `${(s / 60).toFixed(s % 60 ? 1 : 0).replace(".", ",")} min`;
const euros = (v) => `${String(v).replace(".", ",")} €`;

async function render() {
  const r = await authFetch("/api/compte/palier");
  const s = await r.json();
  if (!r.ok) { $("#message").textContent = `⚠ ${s.error || r.status}`; return; }
  $("#usage").innerHTML = `<p>Palier actuel : <strong>${esc(s.plan.label)}</strong></p>
    <p class="muted">Stockage : ${go(s.used_bytes)} sur ${go(s.plan.quota_bytes)}</p>
    <progress value="${Math.min(1, s.used_bytes / s.plan.quota_bytes)}" max="1"></progress>
    <p class="muted">Exports ce mois-ci : ${min(s.export_used_s)} sur ${min(s.plan.export_s)}</p>
    <progress value="${Math.min(1, s.export_used_s / s.plan.export_s)}" max="1"></progress>`;
  $("#plans").innerHTML = s.plans.map((p) => `<li class="lib-row">
    <span class="grow"><strong>${esc(p.label)}</strong> <span class="muted">${p.quota_go} Go · ${p.export_min} min d'export par mois</span></span>
    <span>${p.eur_year ? `${euros(p.eur_year)} par an` : "gratuit"}</span>
    ${p.key === s.plan.key ? `<span class="badge">palier actuel</span>`
      : p.eur_year && s.payment ? `<button class="primary" data-plan="${esc(p.key)}">Choisir</button>` : ""}</li>`).join("");
  if (!s.payment) $("#message").textContent = "Le paiement n'est pas configuré sur ce service.";
  else if (new URLSearchParams(location.search).get("paiement") === "ok") $("#message").textContent = "Paiement reçu : le palier est mis à jour dès la confirmation du prestataire.";
}

$("#plans").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-plan]");
  if (!b) return;
  b.disabled = true;
  const r = await authFetch("/api/paiement/commande", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ plan: b.dataset.plan }) });
  const j = await r.json().catch(() => ({}));
  if (r.ok && j.url) { location.href = j.url; return; }   // page de paiement du prestataire
  $("#message").textContent = `⚠ ${j.error || "commande impossible"}`;
  b.disabled = false;
});
showAccount($("#account"));
render();
