// Essai automatique des paliers, des quotas et du paiement par Mollie, contre le faux serveur local
// (cloud/faux-mollie.mjs) : aucun appel ne part vers Mollie.
// Usage : node cloud/essai-paiement.mjs URL_DU_SERVICE FICHIER_DES_JETONS TABLE
// Le service doit tourner avec un passage fréquent sur les échéances (BIKE360_SWEEP_MIN).
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { mollie, pay, paymentOf, pilot, subscriptionsOf } from "./essai-mollie.mjs";

const [base, tokens, table] = process.argv.slice(2);
const { b } = JSON.parse(readFileSync(tokens));
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const aws = (...args) => execFileSync("aws", [...args, "--output", "json"], { stdio: ["ignore", "pipe", "pipe"] }).toString();
const call = async (method, path, body, token = b.token) => {
  const r = await fetch(`${base}${path}`, { method, headers: { "Content-Type": "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) },
                                          body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => ({})) };
};
const status = async () => (await call("GET", "/api/compte/palier")).body;
const receipts = async () => (await call("GET", "/api/paiement/recus")).body;
/** Notification comme Mollie l'envoie : un formulaire, le seul identifiant du paiement. */
const notify = (id) => fetch(`${base}/api/paiement/mollie`, { method: "POST", headers: { "Content-Type": "application/x-www-form-urlencoded" }, body: new URLSearchParams({ id }) });
const set = (pk, sk, attr, value) => aws("dynamodb", "update-item", "--table-name", table, "--key", JSON.stringify({ pk: { S: pk }, sk: { S: sk } }),
  "--update-expression", "SET #a = :v", "--expression-attribute-names", JSON.stringify({ "#a": attr }), "--expression-attribute-values", JSON.stringify({ ":v": value }));
const inAYear = () => { const d = new Date(); d.setUTCMonth(d.getUTCMonth() + 12); return d.toISOString().slice(0, 10); };
const active = async () => (await subscriptionsOf(b.sub)).filter((s) => s.status === "active");
const big = { name: "VID_20260101_000000_00_001.insv", size: 150e9 };

let s = await status();
check(s.plan.key === "essai" && s.payment && s.provider === "mollie" && s.plans.length === 3,
      `nouveau compte au palier ${s.plan.label} : ${s.plan.quota_bytes / 1e9} Go, ${s.plans.length} paliers proposés, paiement par ${s.provider}`);
let r = await call("POST", "/api/envoi/start", big);
check(r.status === 402, `envoi au-delà de la place du palier refusé : ${r.body.error}`);

// ---- commande d'un palier : premier paiement chez Mollie
check((await call("POST", "/api/paiement/commande", { plan: "essai" })).status === 400, "le palier gratuit ne se commande pas");
check((await call("POST", "/api/paiement/commande", { plan: "200go" }, "")).status === 401, "pas de commande sans connexion");
r = await call("POST", "/api/paiement/commande", { plan: "200go" });
check(r.status === 200 && /^https?:\/\//.test(r.body.url), "commande : Mollie renvoie une page de paiement");
let m = await mollie();
let p = m.payments[paymentOf(r.body.url)];
check(p.sequenceType === "first" && p.amount.value === "25.00" && p.amount.currency === "EUR" && p.customerId && /\/api\/paiement\/mollie$/.test(p.webhookUrl)
   && m.customers[p.customerId].metadata.client === b.sub,
      `chez Mollie : premier paiement de ${p.amount.value} € pour un client rattaché au compte, « ${p.description} »`);

// ---- une notification ne vaut que par ce que Mollie dit du paiement
check((await notify(p.id)).status === 200 && (await status()).plan.key === "essai", "notification d'un paiement encore ouvert : rien ne change");
check((await notify("tr_inconnu")).status === 200 && (await notify("../../compte")).status === 200 && (await status()).plan.key === "essai",
      "notification d'un identifiant inconnu ou fantaisiste : même réponse, rien ne change");
await pay(r.body.url, { status: "failed" });
check((await status()).plan.key === "essai", "carte refusée : le palier n'a pas bougé");
// un paiement que le service n'a pas ouvert, même payé et même s'il nomme le compte, ne change rien
const stray = await (await fetch(`${process.env.BIKE360_MOLLIE_API}/v2/payments`, { method: "POST", headers: { Authorization: "Bearer test_autre", "Content-Type": "application/json" },
  body: JSON.stringify({ amount: { currency: "EUR", value: "0.01" }, description: "intrus", redirectUrl: "http://exemple.invalid/", webhookUrl: p.webhookUrl,
                         metadata: { client: b.sub, nature: "palier", detail: "600go" } }) })).json();
await pilot(`payer/${stray.id}`);
check((await status()).plan.key === "essai", "paiement payé mais que le service n'a pas ouvert : ignoré");
// le montant payé doit être celui que le service attendait
r = await call("POST", "/api/paiement/commande", { plan: "600go" });
set(`paiement#${paymentOf(r.body.url)}`, "commande", "centimes", { N: "1" });
await pay(r.body.url);
check((await status()).plan.key === "essai", "montant payé différent du montant attendu : ignoré");

// ---- paiement confirmé : palier, abonnement annuel chez Mollie, reçu
r = await call("POST", "/api/paiement/commande", { plan: "200go" });
const first = paymentOf(r.body.url);
const paid = await pay(r.body.url);
s = await status();
check(paid.webhook === 200 && s.plan.key === "200go" && s.plan.quota_bytes === 200e9 && s.subscribed && s.standing.state === "abonne",
      `paiement confirmé : palier ${s.plan.label}, ${s.plan.export_s / 60} min d'export par mois`);
check(s.paid_until?.slice(0, 10) === inAYear(), `le compte est payé jusqu'au ${s.paid_until?.slice(0, 10)}`);
let subs = await active();
check(subs.length === 1 && subs[0].interval === "12 months" && subs[0].amount.value === "25.00" && subs[0].startDate === inAYear() && subs[0].mandateId,
      `chez Mollie : un abonnement de ${subs[0]?.amount.value} € tous les ${subs[0]?.interval}, premier prélèvement le ${subs[0]?.startDate}, sur le mandat du premier paiement`);
check((await call("POST", "/api/envoi/start", big)).status !== 402, "le même envoi n'est plus refusé pour la place");
for (let i = 0; i < 3; i++) await notify(first);
let rec = await receipts();
check((await active()).length === 1 && rec.receipts.length === 1 && rec.receipts[0].cents === 2500 && rec.receipts[0].kind === "abonnement",
      `la même notification représentée trois fois : toujours un abonnement et un reçu (${rec.receipts[0]?.number}, ${rec.receipts[0]?.label})`);
check(/293 B du CGI/.test(rec.vat) && rec.seller, `le reçu porte « ${rec.vat} » et le vendeur`);

// ---- moyen de paiement
r = await call("GET", "/api/paiement/moyen");
check(r.status === 200 && /Mastercard se terminant par 4444/.test(r.body.method?.label), `moyen de paiement affiché : ${r.body.method?.label}`);
r = await call("POST", "/api/paiement/moyen");
p = (await mollie()).payments[paymentOf(r.body.url)];
check(r.status === 200 && p.amount.value === "0.00" && p.sequenceType === "first" && p.method === "creditcard", "changer de moyen de paiement : premier paiement de 0 € par carte, rien n'est débité");
await pay(r.body.url);
subs = await active();
check(subs.length === 1 && subs[0].mandateId === (await mollie()).payments[p.id].mandateId && (await receipts()).receipts.length === 1,
      "le nouveau mandat est rattaché à l'abonnement, sans reçu");

// ---- crédit d'export : paiement unique, appliqué une seule fois, même si Mollie est injoignable au premier essai
check((await call("POST", "/api/paiement/credit", { minutes: 59 })).status === 400, "crédit d'export : moins de 60 minutes, refusé");
r = await call("POST", "/api/paiement/credit", { minutes: 60 });
p = (await mollie()).payments[paymentOf(r.body.url)];
check(r.status === 200 && p.sequenceType === "oneoff" && p.amount.value === "4.20", `crédit d'export : paiement unique de ${p.amount.value} € pour 60 minutes`);
await pilot("panne", { calls: 1 });
const down = await pay(r.body.url);
check(down.webhook === 502 && (await status()).credit_s === 0, "Mollie injoignable quand la notification arrive : rien n'est appliqué, le service demande à être rappelé");
const again = await pilot(`notifier/${p.id}`);
await pilot(`notifier/${p.id}`);
s = await status();
rec = await receipts();
check(again.webhook === 200 && s.credit_s === 3600 && rec.receipts.length === 2 && rec.receipts[0].cents === 420,
      `notification représentée : ${s.credit_s / 60} min de crédit, une seule fois ; reçu ${rec.receipts[0]?.number}`);
check(s.plan.key === "200go", "l'achat de crédit ne touche pas au palier");
await pilot(`reprise/${p.id}`, { type: "chargeback" });
await pilot(`notifier/${p.id}`);
s = await status();
rec = await receipts();
check(s.credit_s === 0 && rec.receipts[0].state === "repris" && s.plan.key === "200go", "paiement du crédit contesté auprès de la banque : crédit retiré, reçu marqué repris");

// ---- renouvellement : Mollie prélève l'échéance et notifie
let due = await pilot(`echeance/${subs[0].id}`, { status: "failed" });
s = await status();
check(due.webhook === 200 && s.payment_failed && s.plan.key === "200go" && s.standing.state === "abonne", "renouvellement refusé par la banque : noté, le compte garde son palier pendant le délai de grâce");
due = await pilot(`echeance/${subs[0].id}`);
await pilot(`notifier/${due.id}`);
s = await status();
rec = await receipts();
const inTwoYears = `${Number(inAYear().slice(0, 4)) + 1}${inAYear().slice(4)}`;
check(s.payment_failed === null && s.paid_until.slice(0, 10) === inTwoYears && rec.receipts.length === 3 && rec.receipts[0].kind === "renouvellement",
      `renouvellement payé : compte payé jusqu'au ${s.paid_until.slice(0, 10)}, reçu ${rec.receipts[0]?.number}, une seule fois`);

// ---- changement de palier : l'ancien abonnement ne prélève plus
r = await call("POST", "/api/paiement/commande", { plan: "600go" });
await pay(r.body.url);
s = await status();
subs = await subscriptionsOf(b.sub);
check(s.plan.key === "600go" && subs.filter((x) => x.status === "active").length === 1 && subs.find((x) => x.status === "active").amount.value === "49.00"
   && subs.filter((x) => x.status === "canceled").length === 1,
      "changement de palier : un seul abonnement actif chez Mollie, à 49 €, l'ancien est arrêté");

// ---- résiliation : Mollie n'a pas de résiliation à l'échéance, le service la tient lui-même
r = await call("POST", "/api/paiement/resiliation", {});
s = await status();
check(r.status === 200 && s.cancel_at === s.paid_until && s.plan.key === "600go" && (await active()).length === 0,
      `résiliation : plus aucun prélèvement prévu chez Mollie, le palier court jusqu'au ${s.cancel_at?.slice(0, 10)}`);
r = await call("POST", "/api/paiement/resiliation", { undo: true });
await call("POST", "/api/paiement/resiliation", { undo: true });
s = await status();
subs = await active();
check(r.status === 200 && s.cancel_at === null && subs.length === 1 && subs[0].startDate === s.paid_until.slice(0, 10) && subs[0].amount.value === "49.00",
      `résiliation annulée (deux fois) : un seul abonnement repart chez Mollie, premier prélèvement le ${subs[0]?.startDate}`);
await call("POST", "/api/paiement/resiliation", {});
set(`client#${b.sub}`, "compte", "echeance", { S: new Date(Date.now() - 86400e3).toISOString().slice(0, 19) + "Z" });
set(`client#${b.sub}`, "compte", "resiliation", { S: new Date(Date.now() - 86400e3).toISOString().slice(0, 19) + "Z" });
for (let i = 0; i < 40 && (s = await status()).plan.key !== "essai"; i++) await new Promise((done) => setTimeout(done, 500));
check(s.plan.key === "essai" && s.standing.state === "termine", `échéance passée après la résiliation : retour au palier d'essai (${s.standing.notice})`);

rec = await receipts();
const numbers = rec.receipts.map((x) => Number(x.number.split("-")[1])).reverse();
check(rec.receipts.length === 4 && numbers.every((n, i) => i === 0 || n === numbers[i - 1] + 1),
      `historique : ${rec.receipts.length} reçus numérotés sans trou (${rec.receipts.at(-1).number} à ${rec.receipts[0].number})`);
check((await call("GET", "/api/compte/palier", undefined, "")).status === 401 && (await call("GET", "/api/paiement/recus", undefined, "")).status === 401,
      "ni le palier ni l'historique d'un compte ne se lisent sans connexion");
const calls = (await mollie()).calls;
check(calls.every((c) => c.path.startsWith("/v2/")) && calls.some((c) => c.idempotency?.startsWith("abonnement-tr_")),
      `${calls.length} appels reçus par le faux Mollie, création d'abonnement sous clé d'idempotence`);
