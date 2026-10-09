// Essai automatique de l'atelier à la demande.
// Usage : node cloud/essai-atelier.mjs URL_DU_SERVICE FICHIER_DES_JETONS APERÇU.lrv   (le compte a doit avoir un rush analysé)
import { openAsBlob, readFileSync } from "node:fs";
import { sendFile } from "../ui/envoi-core.js";

const [base, tokens, path] = process.argv.slice(2);
const { a, b, c } = JSON.parse(readFileSync(tokens));
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const bearer = (t) => ({ Authorization: `Bearer ${t}` });
const service = async (who, method, p) => {
  const r = await fetch(`${base}/api/atelier${p}`, { method, headers: bearer(who.token) });
  return { status: r.status, body: await r.json().catch(() => ({})) };
};
/** Ouvre l'atelier d'un compte comme le ferait son navigateur : adresse à usage unique → témoin de session. */
async function open(who) {
  const { body } = await service(who, "POST", "/ouvrir");
  const first = await fetch(body.url, { redirect: "manual" });
  const cookie = (first.headers.getSetCookie()[0] || "").split(";")[0];
  return { url: body.url, origin: new URL(body.url).origin, status: first.status, cookie };
}
const atelier = async (o, method, p, body, cookie = o.cookie) => {
  const r = await fetch(`${o.origin}${p}`, { method, headers: { "Content-Type": "application/json", ...(cookie ? { Cookie: cookie } : {}) },
                                           body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => null) };
};

let st = (await service(a, "GET", "")).body;
check(st.available && !st.running, "atelier proposé, pas encore lancé");
let A = await open(a);
check(A.status === 303 && A.cookie.startsWith("bike360_session="), "ouverture : l'adresse à usage unique donne une session d'atelier");
check((await fetch(A.url, { redirect: "manual" })).status === 403, "la même adresse une seconde fois : refusée");
let sessions = (await atelier(A, "GET", "/api/sessions")).body;
check(Array.isArray(sessions) && sessions.length === 1, `l'atelier du compte montre sa session (${sessions[0]?.duration} s)`);
check((await atelier(A, "GET", "/api/sessions", undefined, "")).status === 401, "sans session, l'atelier refuse");
check((await atelier(A, "GET", "/api/fs?path=/")).status === 404, "atelier en mode hébergé : le disque de la machine n'est pas explorable");
const sid = sessions[0].id;
const clip = { id: "abcd1234", start: 2, end: 8, yaw: 0, pitch: -10, fov: 100, horizon: "fixe" };
check((await atelier(A, "PUT", `/api/selections/${sid}`, [clip])).status === 200, "un clip est posé dans l'atelier");

const B = await open(b);
check(B.origin !== A.origin && (await atelier(B, "GET", "/api/sessions")).body.length === 0, "un autre compte a son propre atelier, sans les rushs du premier");
check((await atelier(A, "GET", "/api/sessions", undefined, B.cookie)).status === 401, "la session d'un compte n'ouvre pas l'atelier d'un autre");

const closed = (await service(a, "POST", "/fermer")).body;
const down = await fetch(`${A.origin}/login`).then(() => false, () => true);
check(closed.saved >= 1 && down, `fermeture : ${closed.saved} fichier(s) enregistré(s), atelier arrêté`);
A = await open(a);
const back = (await atelier(A, "GET", `/api/session/${sid}`)).body;
check(back.selections.length === 1 && back.selections[0].id === clip.id, "réouverture : le clip posé est retrouvé");

// un aperçu qui arrive pendant que l'atelier tourne y apparaît
const C = await open(c);
const blob = await openAsBlob(path);
await sendFile({ name: "LRV_20260920_101500_01_590.lrv", size: blob.size, slice: (x, y) => blob.slice(x, y) }, { base, headers: bearer(c.token) });
let seen = 0;
for (let i = 0; i < 40 && !seen; i++) {
  await new Promise((r) => setTimeout(r, 500));
  seen = (await atelier(C, "GET", "/api/sessions")).body.length;
}
check(seen === 1, "un aperçu envoyé pendant que l'atelier tourne y apparaît sans le relancer");
for (const who of [a, b, c]) await service(who, "POST", "/fermer");
check(!(await service(a, "GET", "")).body.running, "ateliers fermés");
