// Page du palier : place et minutes d'export du compte, grille des paliers, passage au paiement,
// crédit d'export, moyen de paiement, historique et reçus, résiliation et suppression du compte.
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
    ${overage(s)}
    <p class="muted">Exports ce mois-ci : ${min(s.export_used_s)} sur ${min(s.plan.export_s)}${s.credit_s ? ` · ${credits(s.credit_s)}` : ""}</p>
    <progress value="${Math.min(1, s.export_used_s / s.plan.export_s)}" max="1"></progress>`;
  $("#plans").innerHTML = s.plans.map((p) => `<li class="lib-row">
    <span class="grow"><strong>${esc(p.label)}</strong> <span class="muted">${p.quota_go} Go · ${p.export_min} min d'export par mois</span></span>
    <span>${p.eur_year ? `${euros(p.eur_year)} par an` : "gratuit"}</span>
    ${p.key === s.plan.key ? `<span class="badge">palier actuel</span>`
      : p.eur_year && s.payment ? `<button class="primary" data-plan="${esc(p.key)}">Choisir</button>` : ""}</li>`).join("")
    + (s.recovery_eur ? `<li class="muted">Tes rushs sont en archive : reprendre un palier ajoute ${euros(s.recovery_eur)} de frais de récupération.</li>` : "");
  const late = s.payment_failed && s.paid_until ? new Date(new Date(s.paid_until).getTime() + s.grace_days * 86400e3).toISOString() : null;
  $("#subscription").innerHTML = !s.subscribed ? ""
    : s.cancel_at ? `<p>Résiliation demandée : l'abonnement s'arrête le ${esc(day(s.cancel_at))}. <button data-undo="1">Garder mon abonnement</button></p>`
    : (late ? `<p class="notice">Ta banque a refusé le renouvellement de ton abonnement. Il sera représenté dans les prochains jours ; sans paiement
         d'ici le ${esc(day(late))}, l'abonnement s'arrêtera. <button class="primary" data-plan="${esc(s.plan.key)}" data-renew="1">Régler maintenant</button></p>` : "")
      + `<p class="muted">L'abonnement se renouvelle chaque année${s.paid_until ? `, la prochaine fois le ${esc(day(s.paid_until))}` : ""}.
       <button data-undo="">Résilier</button> Il court alors jusqu'à son échéance ;
       tes rushs restent ensuite accessibles ${s.access_days} jours, puis partent en archive.</p>`;
  billing(s);
  $("#credit-balance").textContent = `Solde : ${credits(s.credit_s)}`;
  $("#credit-minutes").min = s.credit.min;
  if (!$("#credit-minutes").value) $("#credit-minutes").value = s.credit.min;
  price();
  $("#credit").hidden = !s.payment;
  if (!s.payment) $("#message").textContent = "Le paiement n'est pas configuré sur ce service.";
  else if (new URLSearchParams(location.search).has("paiement") && !$("#message").textContent)
    $("#message").textContent = "Retour de la page de paiement : le compte est mis à jour dès que le prestataire confirme le paiement.";
}

const credits = (seconds) => `${Math.floor(seconds / 60)} crédit${seconds >= 120 ? "s" : ""}`;

/** Dépassement du quota de stockage : ce qu'il coûte, le délai une fois le crédit épuisé, les rushs partis en archive. */
function overage(s) {
  const o = s.overage;
  if (!o) return "";
  let html = "";
  if (o.bytes > 0) {
    const monthly = Math.ceil(o.bytes / 100e9 * o.credits_100go);
    if (o.out_since) {
      const limit = new Date(new Date(o.out_since).getTime() + o.grace_days * 86400e3).toISOString();
      html += `<p class="notice">Ton stockage dépasse ton quota de ${go(o.bytes)} et tes crédits sont épuisés : l'envoi est suspendu.
        Libère de la place, rachète des crédits ou change de palier avant le ${esc(day(limit))} ; ensuite tes rushs les plus anciens partiront en archive.</p>`;
    } else {
      html += `<p class="muted">Tu dépasses ton quota de ${go(o.bytes)} : cela consomme environ ${monthly} crédit${monthly > 1 ? "s" : ""} par 30 jours
        (${o.credits_100go} crédits par 100 Go), tant que tu n'as pas libéré de place.</p>`;
    }
  }
  if (o.frozen_bytes > 0) {
    html += `<p class="notice">${go(o.frozen_bytes)} de tes rushs les plus anciens sont en archive, faute d'avoir régularisé un dépassement.
      Ils y sont gardés six mois. <button data-recover="1">Les récupérer pour ${Math.ceil(o.recovery_credits)} crédits</button></p>`;
  }
  return html;
}

/** Moyen de paiement de l'abonnement (s'il y en a un et si le prestataire le montre) et historique des paiements. */
async function billing(s) {
  const active = s.payment && s.subscribed && !s.cancel_at;
  const m = active ? await authFetch("/api/paiement/moyen").then((r) => r.json()).catch(() => ({})) : {};
  $("#billing").hidden = !m.method;
  if (m.method) $("#method").textContent = m.method.label + (m.method.status === "pending" ? " (en cours de validation)" : "");
  const h = await authFetch("/api/paiement/recus").then((r) => r.json()).catch(() => ({}));
  $("#receipts").innerHTML = (h.receipts || []).map((x) => `<li class="lib-row">
    <span class="grow">${esc(x.label)} <span class="muted">${esc(day(x.date))} · reçu ${esc(x.number)}${x.state === "repris" ? " · remboursé ou contesté" : ""}</span></span>
    <span>${euros(x.cents / 100)}</span>
    <a class="button" href="recu.html?n=${encodeURIComponent(x.number)}">Reçu</a></li>`).join("") || `<li class="muted">Aucun paiement pour l'instant.</li>`;
}

function price() {
  const n = Number($("#credit-minutes").value) || 0;
  $("#credit-price").textContent = state ? euros(n * state.credit.eur) : "";
}

/** Commande d'un palier : un compte déjà abonné est prévenu que le nouveau palier repart pour un an. */
async function choose(e) {
  const b = e.target.closest("button[data-plan]");
  if (!b) return;
  if (state.subscribed && !b.dataset.renew && state.plan.key !== "essai"
      && !confirm("Le nouveau palier est payé pour un an à compter d'aujourd'hui et remplace l'abonnement en cours, dont la période déjà payée n'est pas remboursée. Continuer ?")) return;
  b.disabled = true;
  const j = await post("/api/paiement/commande", { plan: b.dataset.plan });
  if (j.ok && j.url) { location.href = j.url; return; }   // page de paiement du prestataire
  $("#message").textContent = `⚠ ${j.error || "commande impossible"}`;
  b.disabled = false;
}
$("#plans").addEventListener("click", choose);

$("#method-change").addEventListener("click", async (e) => {
  e.target.disabled = true;
  const j = await post("/api/paiement/moyen", {});
  if (j.ok && j.url) { location.href = j.url; return; }
  $("#message").textContent = `⚠ ${j.error || "changement impossible"}`;
  e.target.disabled = false;
});

$("#subscription").addEventListener("click", async (e) => {
  if (e.target.closest("button[data-plan]")) return choose(e);
  const b = e.target.closest("button[data-undo]");
  if (!b) return;
  b.disabled = true;
  const j = await post("/api/paiement/resiliation", { undo: !!b.dataset.undo });
  $("#message").textContent = j.ok ? "" : `⚠ ${j.error || "demande impossible"}`;
  render();
});

$("#usage").addEventListener("click", async (e) => {
  const b = e.target.closest("button[data-recover]");
  if (!b) return;
  b.disabled = true;
  const j = await post("/api/bibliotheque/recuperer", {});
  $("#message").textContent = j.ok ? "Récupération lancée : tes rushs reviennent de l'archive, ce qui peut demander jusqu'à 48 h." : `⚠ ${j.error || "récupération impossible"}`;
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
// au retour de la page de paiement, la confirmation du prestataire peut suivre de quelques secondes
if (new URLSearchParams(location.search).has("paiement")) for (const wait of [2000, 5000, 10000]) setTimeout(render, wait);
