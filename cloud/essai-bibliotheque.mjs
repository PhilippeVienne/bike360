// Essai automatique de la Bibliothèque contre le service commun.
// Usage : node cloud/essai-bibliotheque.mjs URL_DU_SERVICE FICHIER.lrv   (l'aperçu doit déjà être envoyé)
import { openAsBlob } from "node:fs";
import { basename } from "node:path";
import { sendFile } from "../ui/envoi-core.js";

const [base, path] = process.argv.slice(2);
const blob = await openAsBlob(path);
const check = (ok, text) => { console.log(`   ${ok ? "✓" : "✗"} ${text}`); if (!ok) process.exit(1); };
const call = async (method, p, body) => {
  const r = await fetch(`${base}/api/bibliotheque${p}`, { method, headers: { "Content-Type": "application/json" },
                                                         body: body === undefined ? undefined : JSON.stringify(body) });
  return { status: r.status, body: await r.json().catch(() => ({})) };
};
const only = (lib) => lib.rides[0]?.sessions[0];

// un original pour la même session : même contenu que l'aperçu, donc même caméra dans sa télémétrie
const original = basename(path).replace(/^LRV_(\d{8}_\d{6})_\d{2}/i, "VID_$1_00").replace(/\.lrv$/i, ".insv");
await sendFile({ name: original, size: blob.size, slice: (a, b) => blob.slice(a, b) }, { base });

let lib = (await call("GET", "")).body;
let s = only(lib);
check(lib.rides.length === 1 && lib.rides[0].sessions.length === 1, "une balade, une session : aperçu et original réunis");
check(s.previews.files === 1 && s.originals.files === 1 && lib.bytes === 2 * blob.size, `place occupée : ${lib.bytes} octets pour 2 fichiers`);
check(s.originals.class === "GLACIER_IR" && s.previews.class === "INTELLIGENT_TIERING", "classes de stockage : aperçu et original rangés séparément");
check(lib.suggestions.some((g) => g.session === s.id && g.reason === "courte"), `suggestion : ${lib.suggestions[0]?.text}`);

await call("POST", "/marque", { session: s.id, mark: "favori" });
lib = (await call("GET", "")).body;
check(only(lib).mark === "favori" && lib.suggestions.length === 0, "marqueur favori posé ; la suggestion disparaît");
check((await call("POST", "/marque", { session: s.id, mark: "poubelle" })).status === 400, "marqueur inconnu refusé");

check((await call("POST", "/alleger", { session: s.id })).status === 400, "alléger sans confirmation refusé");
const light = (await call("POST", "/alleger", { session: s.id, confirm: true })).body;
lib = (await call("GET", "")).body;
check(light.freed_bytes === blob.size && only(lib).originals.files === 0 && lib.bytes === blob.size, `allégée : ${light.freed_bytes} octets libérés, l'aperçu reste`);

await call("POST", "/marque", { session: s.id, mark: "corbeille" });
check(only((await call("GET", "")).body).mark === "corbeille", "session à la corbeille, toujours présente");
const purge = (await call("POST", "/purge")).body;
lib = (await call("GET", "")).body;
check(purge.sessions === 1 && lib.rides.length === 0 && lib.bytes === 0, `corbeille vidée : ${purge.freed_bytes} octets libérés, bibliothèque vide`);
