// Page du palier : place et minutes d'export du compte, grille des paliers, passage au paiement,
// crédit d'export, résiliation et suppression du compte.
import { authFetch, showAccount } from "./compte-session.js";

const $ = (s) => document.querySelector(s);
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const go = (b) => `${(b / 1e9).toFixed(b >= 1e11 ? 0 : 1).replace(".", ",")} Go`;
const min = (s) => `${(s / 60).toFixed(s % 60 ? 1 : 0).replace(".", ",")} min`;
const euros = (v) => `${v.toFixed(2).replace(/[.,]00$/, "").replace(".", ",")} €`;
const day = (iso) => new Date(iso).toLocaleDateString("fr-FR", { day: "numeric", month: "long", year: "numeric" });
const post = async (path, body) => {
  const r = await authFetch(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  return { ok: r.ok, ...(await r.json().catch(() => ({}))) };
};
let state = null;   // dernière réponse de /api/compte/palier

async function render() {
  const r = await authFetch("/api/compte/palier");
  const s = await r.json();
  if (!r.ok) { $("#message").textContent = `⚠ ${s.error || r.status}`; return; }
  state = s;
  $("#notice").hidden = !s.standing.notice;
  $("#notice").textContent = s.standing.notice || "";
  $("#usage").innerHTML = `<p>Palier actuel : <strong>${esc(s.plan.label)}</strong></p>
    <p class="muted">Stockage : ${go(s.used_bytes)} sur ${go(s.plan.quota_bytes)}</p>
    <progress value="${Math.min(1, s.used_bytes / s.plan.quota_bytes)}" max="1"></progress>
    <p class="muted">Exports ce mois-ci : ${min(s.export_used_s)} sur ${min(s.plan.export_s)}${s.credit_s ? ` · crédit : ${min(s.credit_s)}` : ""}</p>
    <progress value="${Math.min(1, s.export_used_s / s.plan.export_s)}" max="1"></progress>`;
  $("#plans").innerHTML = s.plans.map((p) => `<li class="lib-row">
    <span class="grow"><strong>${esc(p.label)}</strong> <span class="muted">${p.quota_go} Go · ${p.export_min} min d'export par mois</span></span>
    <span>${p.eur_year ? `${euros(p.eur_year)} par an` : "gratuit"}</span>
    ${p.key === s.plan.key ? `<span class="badge">palier actuel</span>`
      : p.eur_year && s.payment ? `<button class="primary" data-plan="${esc(p.key)}">Choisir</button>` : ""}</li>`).join("")
    + (s.recovery_eur ? `<li class="muted">Tes rushs sont en archive : reprendre un palier ajoute ${euros(s.recovery_eur)} de frais de récupération.</li>` : "");
  $("#subscription").innerHTML = !s.subscribed ? ""
    : s.cancel_at ? `<p>Résiliation demandée : l'abonnement s'arrête le ${esc(day(s.cancel_at))}. <button data-undo="1">Garder mon abonnement</button></p>`
    : `<p class="muted">L'abonnement se renouvelle chaque année. <button data-undo="">Résilier</button> Il court alors jusqu'à son échéance ;
       tes rushs restent ensuite accessibles ${s.access_days} jours, puis partent en archive.</p>`;
  $("#credit-balance").textContent = `Crédit : ${min(s.credit_s)}`;
  $("#credit-minutes").min = s.credit.min;
  if (!$("#credit-minutes").value) $("#credit-minutes").value = s.credit.min;
  price();
  $("#credit").hidden = !s.payment;
  if (!s.payment) $("#message").textContent = "Le paiement n'est pas configuré sur ce service.";
  else if (new URLSearchParams(location.search).get("paiement") === "ok") $("#message").textContent = "Paiement reçu : le compte est mis à jour dès la confirmation du prestataire.";
}

function price() {
  const n = Number($("#credit-minutes").value) || 0;
  $("#credit-price").textContent = state ? euros(n * state.credit.eur) : "";
}

$("#plans").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-plan]");
  if (!b) return;
  b.disabled = true;
  const j = await post("/api/paiement/commande", { plan: b.dataset.plan });
  if (j.ok && j.url) { location.href = j.url; return; }   // page de paiement du prestataire
  $("#message").textContent = `⚠ ${j.error || "commande impossible"}`;
  b.disabled = false;
});

$("#subscription").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-undo]");
  if (!b) return;
  b.disabled = true;
  const j = await post("/api/paiement/resiliation", { undo: !!b.dataset.undo });
  $("#message").textContent = j.ok ? "" : `⚠ ${j.error || "demande impossible"}`;
  render();
});

$("#credit-minutes").addEventListener("input", price);
$("#credit").addEventListener("submit", async (e) => {
  e.preventDefault();
  const j = await post("/api/paiement/credit", { minutes: Number($("#credit-minutes").value) });
  if (j.ok && j.url) { location.href = j.url; return; }
  $("#message").textContent = `⚠ ${j.error || "achat impossible"}`;
});

$("#delete").addEventListener("submit", async (e) => {
  e.preventDefault();
  const j = await post("/api/compte/suppression", { password: $("#delete-password").value });
  if (j.ok) { location.href = "compte.html"; return; }
  $("#message").textContent = `⚠ ${j.error || "suppression impossible"}`;
  $("#message").scrollIntoView({ block: "center" });
});
showAccount($("#account"));
render();
