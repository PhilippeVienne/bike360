// Essai automatique de la vie d'un compte : mot de passe oublié, fin de l'essai, crédit d'export,
// résiliation, archive après un abonnement, récupération, dépassement du quota payé en crédits,
// suppression du compte.
// Usage : node cloud/essai-echeances.mjs URL_DU_SERVICE GROUPE_COGNITO TABLE COMPARTIMENT
// Le paiement passe par le faux serveur Mollie (cloud/faux-mollie.mjs).
// Le service doit tourner avec un passage fréquent sur les échéances (BIKE360_SWEEP_MIN) : les dates
// sont reculées dans l'index, et l'essai attend que le passage suivant en tire les conséquences.
import { execFileSync } from "node:child_process";
import { mollie, pay, paymentOf, pilot, subscriptionsOf } from "./essai-mollie.mjs";

const [base, pool, table, bucket] = process.argv.slice(2);
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const aws = (...args) => execFileSync("aws", [...args, "--output", "json"], { stdio: ["ignore", "pipe", "pipe"] }).toString();
const password = "essai-de-passe-2026";
const stamp = Date.now();

async function call(method, path, body, token) {
  const r = await fetch(`${base}${path}`, { method, body: body === undefined ? undefined : JSON.stringify(body),
    headers: { "Content-Type": "application/json", ...(token ? { Authorization: `Bearer ${token}` } : {}) } });
  const cookie = r.headers.getSetCookie().find((c) => c.startsWith("bike360_acces="));
  return { status: r.status, body: await r.json().catch(() => ({})), token: cookie?.split(";")[0].split("=")[1] };
}
/** Compte confirmé et connecté : {email, token, sub}. */
async function account(name) {
  const email = `${name}-${stamp}@exemple.fr`;
  await call("POST", "/api/compte/inscription", { email, password });
  aws("cognito-idp", "admin-confirm-sign-up", "--user-pool-id", pool, "--username", email);
  const token = (await call("POST", "/api/compte/connexion", { email, password })).token;
  return { email, token, sub: JSON.parse(Buffer.from(token.split(".")[1], "base64url")).sub };
}
const key = (who) => JSON.stringify({ pk: { S: `client#${who.sub}` }, sk: { S: "compte" } });
/** Recule une date de la fiche du compte de `days` jours. */
const backdate = (who, attr, days) => aws("dynamodb", "update-item", "--table-name", table, "--key", key(who), "--update-expression", "SET #a = :d",
  "--expression-attribute-names", JSON.stringify({ "#a": attr }),
  "--expression-attribute-values", JSON.stringify({ ":d": { S: new Date(Date.now() - days * 86400e3).toISOString().slice(0, 19) + "Z" } }));
/** Dépose un rush factice du compte : un objet et sa ligne d'index. */
function rush(who, name) {
  const kind = name.startsWith("LRV") ? "apercus" : "originaux";
  aws("s3api", "put-object", "--bucket", bucket, "--key", `${kind}/${who.sub}/${name}`, "--body", process.argv[1]);
  aws("dynamodb", "put-item", "--table-name", table, "--item", JSON.stringify({ pk: { S: `client#${who.sub}` }, sk: { S: `rush#${name}` },
    cle: { S: `${kind}/${who.sub}/${name}` }, nature: { S: kind }, octets: { N: "1000" }, session: { S: "VID_20260101_000000" }, classe: { S: "STANDARD" } }));
}
const objects = (who) => ["apercus", "originaux", "exports", "donnees"].flatMap((kind) =>
  (JSON.parse(aws("s3api", "list-objects-v2", "--bucket", bucket, "--prefix", `${kind}/${who.sub}/`) || "{}").Contents || []).map((o) => o.Key));
const tagged = (who) => objects(who).filter((k) =>
  JSON.parse(aws("s3api", "get-object-tagging", "--bucket", bucket, "--key", k)).TagSet.some((t) => t.Key === "etat" && t.Value === "archive"));
const rows = (who) => JSON.parse(aws("dynamodb", "query", "--table-name", table, "--key-condition-expression", "pk = :c",
  "--expression-attribute-values", JSON.stringify({ ":c": { S: `client#${who.sub}` } }))).Count;
const standing = async (who) => (await call("GET", "/api/compte/palier", undefined, who.token)).body;
/** Attend que le passage des échéances amène le compte dans cette situation. */
async function until(who, state) {
  let s;
  for (let i = 0; i < 40; i++) {
    s = await standing(who);
    if (s.standing.state === state) return s;
    await new Promise((r) => setTimeout(r, 500));
  }
  return s;
}
const lrv = { name: "LRV_20260101_000000_01_001.lrv", size: 9 };
/** Le compte commande un palier et le paie sur la page de Mollie. */
async function subscribe(who, plan) {
  const order = await call("POST", "/api/paiement/commande", { plan }, who.token);
  await pay(order.body.url);
  return paymentOf(order.body.url);
}
/** Fin d'un abonnement : résilié, puis son échéance passe ; le passage des échéances rend le compte au palier d'essai. */
async function end(who) {
  await call("POST", "/api/paiement/resiliation", {}, who.token);
  backdate(who, "echeance", 1);
  return until(who, "termine");
}
const attribute = (who, attr) => JSON.parse(aws("dynamodb", "get-item", "--table-name", table, "--key", key(who)) || "{}").Item?.[attr]?.S;

// ---- mot de passe oublié
const d = await account("d");
check((await call("POST", "/api/compte/oubli", { email: d.email })).status === 200
   && (await call("POST", "/api/compte/oubli", { email: `inconnu-${stamp}@exemple.fr` })).status === 200,
      "mot de passe oublié : même réponse pour un compte existant et une adresse inconnue");
let r = await call("POST", "/api/compte/reinitialisation", { email: d.email, code: "000000", password: "autre-passe-2026" });
check(r.status === 400, `nouveau mot de passe avec un mauvais code : refusé (${r.body.error})`);
check((await call("POST", "/api/compte/connexion", { email: d.email, password })).status === 200, "l'ancien mot de passe sert toujours");
// le compte a été confirmé par l'administration, sans vérifier son adresse : on la marque vérifiée, comme
// l'aurait fait le code de confirmation ; l'émulateur garde les courriels, où se lit le code envoyé
aws("cognito-idp", "admin-update-user-attributes", "--user-pool-id", pool, "--username", d.email, "--user-attributes", "Name=email_verified,Value=true");
await call("POST", "/api/compte/oubli", { email: d.email });
const mails = await (await fetch(`${process.env.AWS_ENDPOINT_URL}/_aws/ses`)).json();
// le dernier courriel reçu par ce compte (le premier portait le code de confirmation de l'inscription)
const code = (mails.messages || mails).filter((m) => m.Destination.ToAddresses.includes(d.email))
  .sort((x, y) => x.Timestamp.localeCompare(y.Timestamp)).at(-1)?.Body.text_part.match(/\d{6}/)?.[0];
r = await call("POST", "/api/compte/reinitialisation", { email: d.email, code, password: "autre-passe-2026" });
check(r.status === 200, `nouveau mot de passe avec le code reçu par courriel : accepté${r.body.error ? ` (${r.body.error})` : ""}`);
check((await call("POST", "/api/compte/connexion", { email: d.email, password: "autre-passe-2026" })).status === 200
   && (await call("POST", "/api/compte/connexion", { email: d.email, password })).status === 401,
      "le nouveau mot de passe connecte, l'ancien est refusé");

// ---- essai gratuit : 7 jours à compter du premier envoi, puis suppression
let s = await standing(d);
check(s.standing.state === "essai" && s.standing.days_left === null, "nouveau compte : à l'essai, délai pas encore lancé");
check((await call("POST", "/api/envoi/start", lrv, d.token)).status === 200, "premier envoi accepté");
s = await standing(d);
check(s.standing.days_left === 7 && /encore 7 jour/.test(s.standing.notice), `le premier envoi lance l'essai : ${s.standing.notice}`);
rush(d, lrv.name); rush(d, "VID_20260101_000000_00_001.insv");
backdate(d, "essai_debut", 8);
s = await until(d, "essai-termine");
check(s.standing.state === "essai-termine", `huit jours plus tard : ${s.standing.notice}`);
for (let i = 0; i < 40 && objects(d).length; i++) await new Promise((r) => setTimeout(r, 500));
check(objects(d).length === 0 && s.used_bytes === 0 || (await standing(d)).used_bytes === 0, "les rushs de l'essai sont supprimés du stockage et de l'index");
r = await call("POST", "/api/envoi/start", lrv, d.token);
check(r.status === 402, `nouvel envoi refusé : ${r.body.error}`);
check((await call("POST", "/api/atelier/ouvrir", undefined, d.token)).status === 402, "atelier refusé après l'essai");

// ---- abonnement, crédit d'export, résiliation
check((await call("POST", "/api/paiement/resiliation", {}, d.token)).status === 409, "résilier sans abonnement : refusé");
await subscribe(d, "600go");
s = await standing(d);
check(s.standing.state === "abonne" && s.subscribed && (await call("POST", "/api/envoi/start", lrv, d.token)).status === 200, "abonnement payé après l'essai : l'envoi reprend");
check((await call("POST", "/api/paiement/credit", { minutes: 59 }, d.token)).status === 400, "crédit d'export : moins de 60 minutes, refusé");
r = await call("POST", "/api/paiement/credit", { minutes: 60 }, d.token);
check(r.status === 200 && /^https?:\/\//.test(r.body.url), "crédit d'export : page de paiement pour 60 minutes");
await pay(r.body.url);
await pilot(`notifier/${paymentOf(r.body.url)}`);
s = await standing(d);
check(s.credit_s === 3600, `crédit payé : ${s.credit_s / 60} min ; la même notification représentée n'ajoute rien`);
check(s.plan.key === "600go", "l'achat de crédit ne touche pas au palier");
r = await call("POST", "/api/paiement/resiliation", {}, d.token);
s = await standing(d);
check(r.status === 200 && s.cancel_at && s.plan.key === "600go", "résiliation : l'abonnement court jusqu'à son échéance");
await call("POST", "/api/paiement/resiliation", { undo: true }, d.token);
check((await standing(d)).cancel_at === null, "résiliation annulée");

// ---- après l'abonnement : 30 jours d'accès, archive, récupération payante, suppression
rush(d, lrv.name); rush(d, "VID_20260101_000000_00_001.insv");
s = await end(d);
check(s.standing.state === "termine" && s.standing.days_left === 30 && s.credit_s === 3600, `fin de l'abonnement : ${s.standing.notice}`);
check((await call("POST", "/api/envoi/start", { ...lrv, name: "LRV_20260102_000000_01_001.lrv" }, d.token)).status === 402, "plus d'envoi sans abonnement");
const endedAt = attribute(d, "fin_abonnement");
await new Promise((r) => setTimeout(r, 4000));
check(attribute(d, "fin_abonnement") === endedAt, "les passages suivants ne relancent pas le délai d'accès");
backdate(d, "fin_abonnement", 31);
s = await until(d, "archive");
for (let i = 0; i < 40 && tagged(d).length < 2; i++) await new Promise((r) => setTimeout(r, 500));
check(s.standing.state === "archive" && tagged(d).length === 2 && s.recovery_eur === 3,
      `au bout de 31 jours : rushs étiquetés pour l'archive profonde, récupération à ${s.recovery_eur} €`);
check((await call("POST", "/api/atelier/ouvrir", undefined, d.token)).status === 402, "atelier refusé sur des rushs archivés");
r = await call("POST", "/api/paiement/commande", { plan: "600go" }, d.token);
const recovery = (await mollie()).payments[paymentOf(r.body.url)];
check(r.status === 200 && recovery.amount.value === "52.00", `reprise d'un palier : commande de ${recovery.amount.value} €, frais de récupération compris`);
await pay(r.body.url);
s = await until(d, "abonne");
check(s.standing.state === "abonne" && tagged(d).length === 0 && objects(d).length === 2, "abonnement repris : les rushs sortent de l'archive");
await end(d);
backdate(d, "fin_abonnement", 211);
s = await until(d, "supprime");
check(s.standing.state === "supprime" && objects(d).length === 0, `au bout de 211 jours : ${s.standing.notice}`);

// ---- dépassement du quota de stockage, payé en crédits (25 crédits par 100 Go et par 30 jours)
const g = await account("g");
await subscribe(g, "200go");
/** Rush factice de `go` Go, reçu il y a `daysAgo` jours. */
function bigRush(who, name, go, daysAgo) {
  aws("s3api", "put-object", "--bucket", bucket, "--key", `originaux/${who.sub}/${name}`, "--body", process.argv[1]);
  aws("dynamodb", "put-item", "--table-name", table, "--item", JSON.stringify({ pk: { S: `client#${who.sub}` }, sk: { S: `rush#${name}` },
    cle: { S: `originaux/${who.sub}/${name}` }, nature: { S: "originaux" }, octets: { N: String(go * 1e9) }, session: { S: name.slice(0, 19) },
    classe: { S: "GLACIER_IR" }, recu: { S: new Date(Date.now() - daysAgo * 86400e3).toISOString().slice(0, 19) + "Z" } }));
}
async function buyCredits(who, minutes) {
  const order = await call("POST", "/api/paiement/credit", { minutes }, who.token);
  await pay(order.body.url);
}
/** Attend que la fiche du compte vérifie `test` (au passage suivant des échéances). */
async function when(who, test) {
  let now;
  for (let i = 0; i < 40; i++) {
    now = await standing(who);
    if (test(now)) break;
    await new Promise((done) => setTimeout(done, 500));
  }
  return now;
}
const old = "VID_20260301_100000_00_001.insv", fresh = { name: "VID_20260901_100000_00_001.insv", size: 60e9 };
bigRush(g, old, 190, 10);
r = await call("POST", "/api/envoi/start", fresh, g.token);
check(r.status === 402 && /crédit/.test(r.body.error), `190 Go sur 200, envoi de 60 Go sans crédit : ${r.body.error}`);
await buyCredits(g, 60);
check((await call("POST", "/api/envoi/start", fresh, g.token)).status === 200, "avec 60 crédits, le même envoi est accepté au-delà du quota");
bigRush(g, fresh.name, 60, 0);
s = await when(g, () => attribute(g, "depassement_vu"));
check(s.overage.bytes === 50e9 && s.credit_s === 3600, `50 Go au-dessus du quota : le décompte commence, crédit intact (${s.credit_s / 60} crédits)`);
backdate(g, "depassement_vu", 30);
s = await when(g, (x) => x.credit_s < 3600);
check(s.credit_s > 2840 && s.credit_s <= 2850, `30 jours plus tard : 12,5 crédits pris pour 50 Go, il en reste ${(s.credit_s / 60).toFixed(1)}`);
backdate(g, "depassement_vu", 200);
s = await when(g, (x) => x.overage.out_since);
r = await call("POST", "/api/envoi/start", { ...fresh, name: "VID_20260902_100000_00_001.insv" }, g.token);
check(s.credit_s === 0 && s.overage.out_since && r.status === 402 && /épuisé/.test(r.body.error), `crédit épuisé : ${r.body.error}`);
check(tagged(g).length === 0, "pendant le délai de régularisation, rien n'est archivé");
backdate(g, "depassement_fin", 31);
s = await when(g, (x) => x.overage.frozen_bytes > 0);
check(s.overage.frozen_bytes === 190e9 && s.used_bytes === 60e9 && tagged(g).length === 1 && tagged(g)[0].endsWith(old) && s.overage.out_since === null,
      "31 jours sans régulariser : le rush le plus ancien part en archive, le reste tient dans le quota");
check((await call("POST", "/api/envoi/start", { name: "LRV_20260902_100000_01_001.lrv", size: 9 }, g.token)).status === 200, "l'envoi reprend");
r = await call("POST", "/api/bibliotheque/recuperer", undefined, g.token);
check(r.status === 402 && s.overage.recovery_credits === 86, `récupération sans crédit : ${r.body.error}`);
await buyCredits(g, 120);
r = await call("POST", "/api/bibliotheque/recuperer", undefined, g.token);
s = await when(g, (x) => x.standing.state === "abonne" && tagged(g).length === 0);
check(r.status === 200 && r.body.credits === 86 && s.used_bytes === 250e9 && s.overage.frozen_bytes === 0 && tagged(g).length === 0 && s.credit_s <= 34 * 60 && s.credit_s > 33 * 60,
      `récupération payée 86 crédits : les 190 Go reviennent, il reste ${(s.credit_s / 60).toFixed(1)} crédits pour le dépassement`);

// ---- suppression du compte par son titulaire
const e = await account("e");
await subscribe(e, "200go");
const customer = attribute(e, "paiement_client");
rush(e, lrv.name);
aws("s3api", "put-object", "--bucket", bucket, "--key", `donnees/${e.sub}/atelier/projet.json`, "--body", process.argv[1]);
check((await call("POST", "/api/compte/suppression", { password: "pas-le-bon-2026" }, e.token)).status === 403 && objects(e).length === 2,
      "suppression du compte avec un mauvais mot de passe : refusée, rien n'est touché");
r = await call("POST", "/api/compte/suppression", { password }, e.token);
check(r.status === 200 && objects(e).length === 0 && rows(e) === 0, `compte supprimé : ${r.body.files} fichier(s) effacé(s), plus aucune ligne dans l'index`);
check((await call("POST", "/api/compte/connexion", { email: e.email, password })).status === 401, "le compte supprimé ne se connecte plus");
const after = await mollie();
const mapping = JSON.parse(aws("dynamodb", "get-item", "--table-name", table, "--key", JSON.stringify({ pk: { S: `paiement#${customer}` }, sk: { S: "client" } })) || "{}").Item;
check(customer && !after.customers[customer] && (await subscriptionsOf(e.sub)).length === 0 && !mapping
   && Object.values(after.subscriptions).filter((x) => x.customerId === customer).every((x) => x.status === "canceled"),
      "le client est supprimé chez Mollie, son abonnement arrêté, et sa référence de paiement oubliée");

// ---- renouvellement jamais payé : Mollie le représente quelques jours, puis le service met fin à l'abonnement
const f = await account("f");
await subscribe(f, "200go");
const [sub] = await subscriptionsOf(f.sub);
// reconduction tacite : le client en est prévenu par courriel entre trois mois et un mois avant l'échéance
backdate(f, "echeance", -45);
for (let i = 0; i < 40 && !attribute(f, "rappel"); i++) await new Promise((r) => setTimeout(r, 500));
const sent = await (await fetch(`${process.env.AWS_ENDPOINT_URL}/_aws/ses`)).json();
const reminder = (sent.messages || sent).filter((x) => x.Destination.ToAddresses.includes(f.email)).map((x) => x.Body.text_part).find((t) => /se renouvellera/.test(t));
check(attribute(f, "rappel") && /25 €/.test(reminder || "") && /résilier/.test(reminder || ""), "45 jours avant l'échéance : courriel annonçant le renouvellement, son prix et la façon de résilier");
await new Promise((r) => setTimeout(r, 4000));
const all = await (await fetch(`${process.env.AWS_ENDPOINT_URL}/_aws/ses`)).json();
check((all.messages || all).filter((x) => x.Destination.ToAddresses.includes(f.email) && /se renouvellera/.test(x.Body.text_part)).length === 1, "ce rappel ne part qu'une fois par échéance");
await pilot(`echeance/${sub.id}`, { status: "failed" });
s = await standing(f);
check(s.payment_failed && s.standing.state === "abonne", "renouvellement refusé : le compte reste abonné pendant le délai de grâce");
backdate(f, "echeance", 10);
await new Promise((r) => setTimeout(r, 4000));
check((await standing(f)).standing.state === "abonne", "dix jours après l'échéance : toujours abonné");
backdate(f, "echeance", 15);
s = await until(f, "termine");
check(s.standing.state === "termine" && s.payment_failed === null && (await subscriptionsOf(f.sub)).every((x) => x.status === "canceled"),
      `quinze jours après l'échéance impayée : abonnement terminé, arrêté chez Mollie (${s.standing.notice})`);
