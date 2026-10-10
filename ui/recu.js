// Reçu d'un paiement, à imprimer : vendeur, client, désignation, montant et mention de TVA.
import { authFetch } from "./compte-session.js";

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const euros = (cents) => `${(cents / 100).toFixed(2).replace(".", ",")} €`;
const day = (iso) => new Date(iso).toLocaleDateString("fr-FR", { day: "numeric", month: "long", year: "numeric" });
const methods = { creditcard: "carte bancaire", directdebit: "prélèvement SEPA", paypal: "PayPal", ideal: "iDEAL", bancontact: "Bancontact" };

const number = new URLSearchParams(location.search).get("n");
const [h, account] = await Promise.all([authFetch("/api/paiement/recus").then((r) => r.json()), fetch("/api/compte").then((r) => r.json()).catch(() => ({}))]);
const x = (h.receipts || []).find((r) => r.number === number);
document.querySelector("#receipt").innerHTML = !x ? `<p>Reçu introuvable.</p>` : `
  <h1>Reçu n° ${esc(x.number)}</h1>
  <p class="muted">Paiement du ${esc(day(x.date))}${x.method ? `, par ${esc(methods[x.method] || x.method)}` : ""}</p>
  <div class="parties">
    <div><strong>Vendeur</strong>\n${esc(h.seller ? h.seller.split(/,\s*/).join("\n") : "Bike360 Cloud")}</div>
    <div><strong>Client</strong>\n${esc(account.email || "")}</div>
  </div>
  <table>
    <tr><th>Désignation</th><th>Montant</th></tr>
    <tr><td>Bike360 Cloud : ${esc(x.label)}</td><td>${euros(x.cents)}</td></tr>
    <tr><th>Total payé</th><th>${euros(x.cents)}</th></tr>
  </table>
  <p>${esc(h.vat)}.</p>
  ${x.state === "repris" ? `<p><strong>Ce paiement a été remboursé ou contesté : il n'est plus acquis.</strong></p>` : ""}
  <p class="muted">Référence du paiement chez le prestataire : ${esc(x.reference)}</p>`;
document.title = `Bike360 — reçu ${number || ""}`;
