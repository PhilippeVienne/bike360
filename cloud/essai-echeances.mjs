// Essai automatique de la vie d'un compte : mot de passe oublié, fin de l'essai, crédit d'export,
// résiliation, archive après un abonnement, récupération, suppression du compte.
// Usage : node cloud/essai-echeances.mjs URL_DU_SERVICE GROUPE_COGNITO TABLE COMPARTIMENT SECRET_DE_NOTIFICATION
// Le service doit tourner avec un passage fréquent sur les échéances (BIKE360_SWEEP_MIN) : les dates
// sont reculées dans l'index, et l'essai attend que le passage suivant en tire les conséquences.
import { execFileSync } from "node:child_process";
import { createHmac } from "node:crypto";

const [base, pool, table, bucket, secret] = process.argv.slice(2);
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
async function notify(event) {
  const body = JSON.stringify(event), t = Math.floor(Date.now() / 1000);
  const sig = createHmac("sha256", secret).update(`${t}.${body}`).digest("hex");
  const r = await fetch(`${base}/api/paiement/stripe`, { method: "POST", headers: { "Stripe-Signature": `t=${t},v1=${sig}` }, body });
  return (await r.json().catch(() => ({}))).result;
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
const paid = (who, plan, n) => ({ type: "checkout.session.completed", data: { object: {
  client_reference_id: who.sub, metadata: { palier: plan }, payment_status: "paid", customer: `cus_${n}${stamp}`, subscription: `sub_${n}${stamp}` } } });

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
await notify(paid(d, "600go", "d"));
s = await standing(d);
check(s.standing.state === "abonne" && s.subscribed && (await call("POST", "/api/envoi/start", lrv, d.token)).status === 200, "abonnement payé après l'essai : l'envoi reprend");
check((await call("POST", "/api/paiement/credit", { minutes: 9 }, d.token)).status === 400, "crédit d'export : moins de 10 minutes, refusé");
r = await call("POST", "/api/paiement/credit", { minutes: 10 }, d.token);
check(r.status === 200 && /^https?:\/\//.test(r.body.url), "crédit d'export : page de paiement pour 10 minutes");
const credit = { type: "checkout.session.completed", data: { object: { id: `cs_${stamp}`, client_reference_id: d.sub, metadata: { credit_min: "10" }, payment_status: "paid" } } };
const first = await notify(credit), again = await notify(credit);
s = await standing(d);
check(first === "crédit ajouté" && again.startsWith("ignorée") && s.credit_s === 600, `crédit payé : ${s.credit_s / 60} min ; la même notification représentée n'ajoute rien`);
check(s.plan.key === "600go", "l'achat de crédit ne touche pas au palier");
r = await call("POST", "/api/paiement/resiliation", {}, d.token);
s = await standing(d);
check(r.status === 200 && s.cancel_at && s.plan.key === "600go", "résiliation : l'abonnement court jusqu'à son échéance");
await call("POST", "/api/paiement/resiliation", { undo: true }, d.token);
check((await standing(d)).cancel_at === null, "résiliation annulée");

// ---- après l'abonnement : 30 jours d'accès, archive, récupération payante, suppression
rush(d, lrv.name); rush(d, "VID_20260101_000000_00_001.insv");
const ended = { type: "customer.subscription.deleted", data: { object: { customer: `cus_d${stamp}` } } };
await notify(ended);
s = await standing(d);
check(s.standing.state === "termine" && s.standing.days_left === 30 && s.credit_s === 600, `fin de l'abonnement : ${s.standing.notice}`);
check((await call("POST", "/api/envoi/start", { ...lrv, name: "LRV_20260102_000000_01_001.lrv" }, d.token)).status === 402, "plus d'envoi sans abonnement");
check((await notify(ended)).startsWith("ignorée"), "la fin d'abonnement représentée ne relance pas le délai");
backdate(d, "fin_abonnement", 31);
s = await until(d, "archive");
for (let i = 0; i < 40 && tagged(d).length < 2; i++) await new Promise((r) => setTimeout(r, 500));
check(s.standing.state === "archive" && tagged(d).length === 2 && s.recovery_eur === 1.5,
      `au bout de 31 jours : rushs étiquetés pour l'archive profonde, récupération à ${s.recovery_eur} €`);
check((await call("POST", "/api/atelier/ouvrir", undefined, d.token)).status === 402, "atelier refusé sur des rushs archivés");
check((await call("POST", "/api/paiement/commande", { plan: "600go" }, d.token)).status === 200, "reprise d'un palier : commande avec les frais de récupération");
await notify(paid(d, "600go", "d"));
s = await until(d, "abonne");
check(s.standing.state === "abonne" && tagged(d).length === 0 && objects(d).length === 2, "abonnement repris : les rushs sortent de l'archive");
await notify(ended);
backdate(d, "fin_abonnement", 211);
s = await until(d, "supprime");
check(s.standing.state === "supprime" && objects(d).length === 0, `au bout de 211 jours : ${s.standing.notice}`);

// ---- suppression du compte par son titulaire
const e = await account("e");
await notify(paid(e, "200go", "e"));
rush(e, lrv.name);
aws("s3api", "put-object", "--bucket", bucket, "--key", `donnees/${e.sub}/atelier/projet.json`, "--body", process.argv[1]);
check((await call("POST", "/api/compte/suppression", { password: "pas-le-bon-2026" }, e.token)).status === 403 && objects(e).length === 2,
      "suppression du compte avec un mauvais mot de passe : refusée, rien n'est touché");
r = await call("POST", "/api/compte/suppression", { password }, e.token);
check(r.status === 200 && objects(e).length === 0 && rows(e) === 0, `compte supprimé : ${r.body.files} fichier(s) effacé(s), plus aucune ligne dans l'index`);
check((await call("POST", "/api/compte/connexion", { email: e.email, password })).status === 401, "le compte supprimé ne se connecte plus");
check((await notify({ type: "customer.subscription.deleted", data: { object: { customer: `cus_e${stamp}` } } })).startsWith("ignorée"),
      "la référence de paiement du compte supprimé est oubliée");
