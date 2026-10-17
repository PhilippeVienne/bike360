// Essai automatique du module Envoi : le code du navigateur (ui/envoi-core.js) contre le service
// d'envoi et le stockage. Usage : node cloud/essai-envoi.mjs URL_DU_SERVICE FICHIER.lrv
import { openAsBlob } from "node:fs";
import { basename } from "node:path";
import { sendFile, sessionsOf, queueOf } from "../ui/envoi-core.js";

const [base, path] = process.argv.slice(2);
// compte connecté (essai avec comptes activés) : le jeton accompagne les appels au service
const headers = process.env.BIKE360_TOKEN ? { Authorization: `Bearer ${process.env.BIKE360_TOKEN}` } : {};
const blob = await openAsBlob(path);
const file = { name: basename(path), size: blob.size, slice: (a, b) => blob.slice(a, b) };
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };

// compte les morceaux réellement déposés dans le stockage
const realFetch = globalThis.fetch;
let puts = 0;
globalThis.fetch = (url, init) => { if (init && init.method === "PUT") puts++; return realFetch(url, init); };

const plan = queueOf(sessionsOf([file, { name: "notes.txt", size: 3 }]));
check(plan.length === 1 && plan[0] === file, "seuls les fichiers de la caméra sont retenus");

// 1. coupure après le premier morceau (comme un onglet fermé)
const cut = new AbortController();
let interrupted = false;
await sendFile(file, { base, headers, signal: cut.signal, concurrency: 1, retries: 0, onProgress: (p) => { if (p > 0) cut.abort(new Error("coupure")); } })
  .catch(() => { interrupted = true; });
const afterCut = puts;
check(interrupted && afterCut >= 1, `envoi coupé après ${afterCut} morceau(x)`);

// 2. reprise : seuls les morceaux manquants repartent
puts = 0;
const resumed = await sendFile(file, { base, headers });
check(resumed.skipped >= 1 && puts === resumed.sent, `reprise : ${resumed.skipped} morceau(x) déjà en place, ${resumed.sent} envoyé(s)`);
check(!!resumed.camera && resumed.duration_s > 0, `télémétrie lue à l'arrivée : ${resumed.camera}, ${resumed.duration_s && resumed.duration_s.toFixed(1)} s`);
check(resumed.indexed === true, "rush inscrit dans l'index");

// 3. fichier complet : rien n'est renvoyé
puts = 0;
const again = await sendFile(file, { base, headers });
check(again.already && puts === 0, "troisième passage : fichier déjà complet, aucun octet renvoyé");

const list = await (await realFetch(`${base}/api/envoi/rushs`, { headers })).json();
const mine = list.find((r) => r.name === file.name);
check(mine && mine.size === file.size, `rush listé : ${mine && mine.kind}, ${mine && mine.size} octets, classe ${mine && mine.class}`);

const bad = await realFetch(`${base}/api/envoi/start`, { method: "POST", headers: { "Content-Type": "application/json", ...headers },
                                                         body: JSON.stringify({ name: "../../etc/passwd", size: 10 }) });
check(bad.status === 400, "nom de fichier hors format refusé");
