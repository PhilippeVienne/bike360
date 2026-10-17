// Essai automatique des comptes : inscription, confirmation, connexion, témoins, rafraîchissement.
// Usage : node cloud/essai-comptes.mjs URL_DU_SERVICE GROUPE_COGNITO FICHIER_DES_JETONS
// Écrit les jetons de trois comptes (a, b et c) pour les essais suivants.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";

const [base, pool, out] = process.argv.slice(2);
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const stamp = Date.now();
const password = "essai-de-passe-2026";

async function call(method, path, body, cookies = {}) {
  const headers = { "Content-Type": "application/json" };
  if (Object.keys(cookies).length) headers.Cookie = Object.entries(cookies).map(([k, v]) => `${k}=${v}`).join("; ");
  const r = await fetch(`${base}${path}`, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) });
  const set = {};
  for (const c of r.headers.getSetCookie()) {
    const [pair, ...attrs] = c.split(";").map((x) => x.trim());
    const [name, value] = pair.split("=");
    set[name] = { value, attrs: attrs.map((a) => a.toLowerCase()) };
  }
  return { status: r.status, body: await r.json().catch(() => ({})), set };
}
// l'émulateur n'envoie pas de courriel : la confirmation passe par l'administration du groupe
const confirmByAdmin = (email) => execFileSync("aws", ["cognito-idp", "admin-confirm-sign-up", "--user-pool-id", pool, "--username", email]);
const subOf = (token) => JSON.parse(Buffer.from(token.split(".")[1], "base64url")).sub;

const a = `a-${stamp}@exemple.fr`, b = `b-${stamp}@exemple.fr`;
let r = await call("GET", "/api/compte");
check(r.body.auth === true && r.body.signed_in === false, "comptes activés, visiteur non connecté");
check((await call("GET", "/api/bibliotheque")).status === 401 && (await call("POST", "/api/envoi/start", { name: "LRV_20260101_000000_01_001.lrv", size: 9 })).status === 401,
      "sans connexion, la Bibliothèque et l'Envoi répondent 401");

r = await call("POST", "/api/compte/inscription", { email: a, password });
check(r.status === 200 && r.body.confirmed === false, "inscription : compte créé, à confirmer");
check((await call("POST", "/api/compte/inscription", { email: a, password })).status === 409, "même adresse une seconde fois : refusée");
check((await call("POST", "/api/compte/connexion", { email: a, password })).status === 403, "connexion avant confirmation : refusée");
check((await call("POST", "/api/compte/confirmation", { email: a, code: "000000" })).status === 400, "mauvais code de confirmation : refusé");
confirmByAdmin(a);
check((await call("POST", "/api/compte/connexion", { email: a, password: "autre-chose-2026" })).status === 401, "mauvais mot de passe : refusé");

r = await call("POST", "/api/compte/connexion", { email: a, password });
const acc = r.set.bike360_acces, ref = r.set.bike360_suite;
check(r.status === 200 && acc && ref, "connexion : jetons posés dans deux témoins");
check([acc, ref].every((c) => c.attrs.includes("httponly") && c.attrs.includes("samesite=strict")) && ref.attrs.includes("path=/api/compte"),
      "témoins illisibles par la page (HttpOnly), jamais envoyés depuis un autre site, jeton de suite limité à /api/compte");
const tokenA = acc.value;
r = await call("GET", "/api/compte", undefined, { bike360_acces: tokenA });
check(r.body.signed_in && r.body.email === a, `compte reconnu par son témoin : ${r.body.email.replace(/-\d+/, "")}`);
check((await call("GET", "/api/bibliotheque", undefined, { bike360_acces: tokenA })).status === 200, "Bibliothèque accessible une fois connecté");

const [h, p, s] = tokenA.split(".");
const forged = [h, Buffer.from(JSON.stringify({ ...JSON.parse(Buffer.from(p, "base64url")), sub: "autre-client" })).toString("base64url"), s].join(".");
check((await call("GET", "/api/bibliotheque", undefined, { bike360_acces: forged })).status === 401, "jeton falsifié (autre client, même signature) : refusé");

r = await call("POST", "/api/compte/rafraichir", undefined, { bike360_suite: ref.value });
check(r.status === 200 && r.set.bike360_acces, "session prolongée par le jeton de suite");
check((await call("POST", "/api/compte/rafraichir")).status === 401, "prolongation sans jeton de suite : refusée");
r = await call("POST", "/api/compte/deconnexion", undefined, { bike360_acces: tokenA });
check(r.set.bike360_acces?.attrs.includes("max-age=0") && r.set.bike360_suite?.attrs.includes("max-age=0"), "déconnexion : les deux témoins sont effacés");

await call("POST", "/api/compte/inscription", { email: b, password });
confirmByAdmin(b);
const tokenB = (await call("POST", "/api/compte/connexion", { email: b, password })).set.bike360_acces.value;
check(subOf(tokenA) !== subOf(tokenB), "deux comptes, deux identifiants de client distincts");
const cMail = `c-${stamp}@exemple.fr`;   // troisième compte, pour les essais qui lui déposent des rushs
await call("POST", "/api/compte/inscription", { email: cMail, password });
confirmByAdmin(cMail);
const tokenC = (await call("POST", "/api/compte/connexion", { email: cMail, password })).set.bike360_acces.value;
const entry = (token) => ({ token, sub: subOf(token) });
writeFileSync(out, JSON.stringify({ a: entry(tokenA), b: entry(tokenB), c: entry(tokenC) }));
