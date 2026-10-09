// Essai automatique des paliers, des quotas et du paiement (notifications signées).
// Usage : node cloud/essai-paiement.mjs URL_DU_SERVICE FICHIER_DES_JETONS SECRET_DE_NOTIFICATION
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";

const [base, tokens, secret] = process.argv.slice(2);
const { b } = JSON.parse(readFileSync(tokens));
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const call = async (method, path, body, token = b.token) => {
  const r = await fetch(`${base}${path}`, { method, headers: { "Content-Type": "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) },
                                          body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => ({})) };
};
/** Notification telle que le prestataire l'envoie : corps brut et signature de « horodatage.corps ». */
const notify = async (event, { key = secret, t = Math.floor(Date.now() / 1000) } = {}) => {
  const body = JSON.stringify(event);
  const sig = createHmac("sha256", key).update(`${t}.${body}`).digest("hex");
  const r = await fetch(`${base}/api/paiement/stripe`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body });
  return { status: r.status, body: await r.json().catch(() => ({})) };
};
const paid = (plan, extra = {}) => ({ type: "checkout.session.completed", data: { object: {
  client_reference_id: b.sub, metadata: { palier: plan }, payment_status: "paid", customer: "cus_essai1", subscription: "sub_essai1", ...extra } } });
const big = { name: "VID_20260101_000000_00_001.insv", size: 150e9 };

let s = (await call("GET", "/api/compte/palier")).body;
check(s.plan.key === "essai" && s.payment && s.plans.length === 5, `nouveau compte au palier ${s.plan.label} : ${s.plan.quota_bytes / 1e9} Go, ${s.plans.length} paliers proposés`);
let r = await call("POST", "/api/envoi/start", big);
check(r.status === 402, `envoi au-delà de la place du palier refusé : ${r.body.error}`);

check((await call("POST", "/api/paiement/commande", { plan: "essai" })).status === 400, "le palier gratuit ne se commande pas");
r = await call("POST", "/api/paiement/commande", { plan: "600go" });
check(r.status === 200 && /^https?:\/\//.test(r.body.url), "commande : le prestataire renvoie une page de paiement");

check((await notify(paid("600go"), { key: "whsec_autre" })).status === 400, "notification signée d'un autre secret : refusée");
check((await notify(paid("600go"), { t: Math.floor(Date.now() / 1000) - 3600 })).status === 400, "notification vieille d'une heure : refusée");
check((await call("GET", "/api/compte/palier")).body.plan.key === "essai", "le palier n'a pas bougé");
check((await notify(paid("600go", { payment_status: "unpaid" }))).body.result?.startsWith("ignorée"), "paiement non abouti : ignoré");
check((await notify(paid("palier-inventé"))).body.result?.startsWith("ignorée"), "palier inconnu : ignoré");

r = await notify(paid("600go"));
s = (await call("GET", "/api/compte/palier")).body;
check(r.status === 200 && s.plan.key === "600go" && s.plan.quota_bytes === 600e9, `paiement confirmé : palier ${s.plan.label}, ${s.plan.export_s / 60} min d'export par mois`);
check((await call("POST", "/api/envoi/start", big)).status !== 402, "le même envoi n'est plus refusé pour la place");

r = await notify({ type: "customer.subscription.deleted", data: { object: { customer: "cus_essai1" } } });
check(r.status === 200 && (await call("GET", "/api/compte/palier")).body.plan.key === "essai", "fin de l'abonnement : retour au palier d'essai");
check((await call("GET", "/api/compte/palier", undefined, "")).status === 401, "le palier d'un compte ne se lit pas sans connexion");
