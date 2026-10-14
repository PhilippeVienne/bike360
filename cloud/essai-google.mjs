// Essai automatique de la connexion par Google : départ, retour, témoins, refus.
// Usage : node cloud/essai-google.mjs URL_DU_SERVICE GROUPE_COGNITO URL_DU_FAUX_OAUTH
// L'émulateur n'a ni domaine OAuth ni fédération : cloud/faux-oauth.mjs joue Cognito et rend les
// jetons d'un compte ouvert ici par mot de passe. Ce que l'essai ne peut donc pas montrer : la
// création du compte par Google, le refus d'un doublon et la suppression d'un compte sans mot de passe.
import { execFileSync } from "node:child_process";

const [base, pool, oauth] = process.argv.slice(2);
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const email = `g-${Date.now()}@exemple.fr`, password = "essai-de-passe-2026";

/** Requête sans suivre les renvois ; rend le statut, l'adresse du renvoi et les témoins posés. */
async function hop(url, cookies = {}, init = {}) {
  const headers = { ...init.headers };
  if (Object.keys(cookies).length) headers.Cookie = Object.entries(cookies).map(([k, v]) => `${k}=${v}`).join("; ");
  const r = await fetch(url, { redirect: "manual", ...init, headers });
  const set = {};
  for (const c of r.headers.getSetCookie()) {
    const [pair, ...attrs] = c.split(";").map((x) => x.trim());
    const i = pair.indexOf("=");
    set[pair.slice(0, i)] = { value: pair.slice(i + 1), attrs: attrs.map((a) => a.toLowerCase()) };
  }
  return { status: r.status, to: r.headers.get("location"), set, body: await r.json().catch(() => ({})) };
}
const json = (body) => ({ method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });

let r = await hop(`${base}/api/compte`);
check(r.body.google === true, "le service propose la connexion par Google");

// le compte dont le faux Cognito rendra les jetons
await hop(`${base}/api/compte/inscription`, {}, json({ email, password }));
execFileSync("aws", ["cognito-idp", "admin-confirm-sign-up", "--user-pool-id", pool, "--username", email]);
r = await hop(`${base}/api/compte/connexion`, {}, json({ email, password }));
const tokens = { access_token: r.set.bike360_acces.value, refresh_token: r.set.bike360_suite.value, expires_in: 3600 };
await fetch(`${oauth}/_jetons`, json(tokens));

/** Départ vers Google puis passage chez le faux Cognito : rend le témoin du trajet et l'adresse du retour. */
async function leave() {
  const start = await hop(`${base}/api/compte/google`);
  const there = await hop(start.to);
  return { start, transit: start.set.bike360_google, back: there.to };
}

let trip = await leave();
const q = new URL(trip.start.to).searchParams;
check(trip.start.status === 303 && trip.start.to.startsWith(`${oauth}/oauth2/authorize?`) && q.get("identity_provider") === "Google"
      && q.get("redirect_uri") === `${base}/api/compte/google/retour`, "départ : renvoi vers l'interface OAuth, fournisseur Google, retour au service");
check(q.get("code_challenge_method") === "S256" && q.get("code_challenge").length === 43 && q.get("state").length === 43, "état et défi PKCE tirés au hasard");
check(trip.transit && trip.transit.attrs.includes("httponly") && trip.transit.attrs.includes("samesite=lax") && trip.transit.attrs.includes("path=/api/compte/google"),
      "témoin du trajet illisible par la page, limité au trajet, renvoyé au retour d'un autre site");
check(trip.back && trip.back.startsWith(`${base}/api/compte/google/retour?code=`), "le faux Cognito renvoie au service avec un code");

r = await hop(trip.back, { bike360_google: trip.transit.value });
check(r.status === 303 && r.to === "/ui/bibliotheque.html" && r.set.bike360_acces && r.set.bike360_suite, "retour : témoins de session posés, renvoi vers la Bibliothèque");
check(r.set.bike360_acces.attrs.includes("samesite=strict") && r.set.bike360_google?.attrs.includes("max-age=0"), "témoin du trajet effacé, témoins de session inchangés dans leur forme");
const me = await hop(`${base}/api/compte`, { bike360_acces: r.set.bike360_acces.value });
check(me.body.signed_in && me.body.email === email && me.body.password === true, "compte reconnu après le retour");

r = await hop(trip.back, { bike360_google: trip.transit.value });
check(r.to === "/ui/compte.html?erreur=google" && !r.set.bike360_acces, "même code une seconde fois : refusé");
trip = await leave();
r = await hop(trip.back);
check(r.to === "/ui/compte.html?erreur=google" && !r.set.bike360_acces, "retour sans le témoin du trajet : refusé");
const other = await leave();
r = await hop(trip.back, { bike360_google: other.transit.value });
check(r.to === "/ui/compte.html?erreur=google" && !r.set.bike360_acces, "retour avec l'état d'un autre trajet : refusé");
const [state] = other.transit.value.split(".");
r = await hop(other.back, { bike360_google: `${state}.${"x".repeat(43)}` });
check(r.to === "/ui/compte.html?erreur=google" && !r.set.bike360_acces, "mauvais vérificateur PKCE : refusé");
r = await hop(`${base}/api/compte/google/retour`);
check(r.to === "/ui/compte.html?erreur=google", "retour sans code : refusé");
