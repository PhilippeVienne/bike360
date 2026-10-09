// Envoi d'un rush vers le stockage, par morceaux et avec reprise. Sans dépendance à la page :
// les mêmes fonctions servent au navigateur (envoi.js) et à l'essai automatique (cloud/essai-envoi.mjs).
// Le service d'envoi ouvre l'envoi et signe l'adresse de chaque morceau ; les octets vont
// directement du navigateur au stockage.

export const RUSH_RE = /^(VID|LRV)_(\d{8})_(\d{6})_(\d{2})_(\d{3})\.(insv|lrv)$/i;
const URLS_PER_CALL = 50;       // adresses signées demandées à la fois
const RETRY_WAIT_MS = 1500;     // attente avant de renvoyer un morceau refusé (doublée à chaque essai)

/** Fichiers de la caméra parmi `files`, groupés par session : [{id, date, time, previews, originals, bytes}]. */
export function sessionsOf(files) {
  const by = new Map();
  for (const f of files) {
    const m = RUSH_RE.exec(f.name);
    if (!m) continue;
    const id = `VID_${m[2]}_${m[3]}`;
    if (!by.has(id)) by.set(id, { id, date: m[2], time: m[3], previews: [], originals: [], bytes: 0 });
    const s = by.get(id);
    (m[1].toUpperCase() === "LRV" ? s.previews : s.originals).push(f);
    s.bytes += f.size;
  }
  return [...by.values()].sort((a, b) => b.id.localeCompare(a.id));
}

/** Ordre d'envoi : tous les aperçus d'abord (on peut trier et monter sans attendre), puis les originaux. */
export function queueOf(sessions, { originals = true } = {}) {
  const byName = (a, b) => a.name.localeCompare(b.name);
  const previews = sessions.flatMap((s) => s.previews).sort(byName);
  return originals ? previews.concat(sessions.flatMap((s) => s.originals).sort(byName)) : previews;
}

async function call(base, path, body, signal) {
  const r = await fetch(`${base}/api/envoi/${path}`, { method: "POST", signal, headers: { "Content-Type": "application/json" },
                                                       body: JSON.stringify(body) });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || `${path} → ${r.status}`);
  return j;
}

const wait = (ms, signal) => new Promise((resolve, reject) => {
  const t = setTimeout(resolve, ms);
  if (signal) signal.addEventListener("abort", () => { clearTimeout(t); reject(signal.reason); }, { once: true });
});

/**
 * Envoie un fichier ; ne renvoie que les morceaux que le stockage n'a pas déjà.
 * `onProgress(octets déjà en place)` est appelé à l'ouverture puis après chaque morceau.
 * Résultat : {key, sent (morceaux envoyés), skipped (morceaux déjà en place), camera, duration_s, indexed}.
 */
export async function sendFile(file, { base = "", onProgress = () => {}, signal, concurrency = 3, retries = 3 } = {}) {
  const head = { name: file.name, size: file.size };
  const st = await call(base, "start", head, signal);
  if (st.done) { onProgress(file.size); return { key: st.key, sent: 0, skipped: 0, already: true }; }
  const len = (n) => Math.min(st.part_size, file.size - (n - 1) * st.part_size);
  const have = new Set(st.received);
  const todo = [];
  for (let n = 1; n <= st.parts; n++) if (!have.has(n)) todo.push(n);
  let placed = st.received.reduce((a, n) => a + len(n), 0);
  onProgress(placed);

  let urls = {};
  const urlFor = async (n) => {
    if (!urls[n]) {   // lot d'adresses à partir de ce morceau
      const from = todo.indexOf(n);
      urls = (await call(base, "urls", { name: file.name, upload_id: st.upload_id, parts: todo.slice(from, from + URLS_PER_CALL) }, signal)).urls;
    }
    return urls[n];
  };
  const put = async (n) => {
    const start = (n - 1) * st.part_size;
    for (let attempt = 0; ; attempt++) {
      try {
        const r = await fetch(await urlFor(n), { method: "PUT", body: file.slice(start, start + len(n)), signal });
        if (r.ok) return;
        throw new Error(`morceau ${n} refusé (${r.status})`);
      } catch (e) {
        if ((signal && signal.aborted) || attempt >= retries) throw e;
        delete urls[n];   // l'adresse a pu expirer
        await wait(RETRY_WAIT_MS * 2 ** attempt, signal);
      }
    }
  };
  let next = 0;
  const worker = async () => {
    while (next < todo.length) {
      const n = todo[next++];
      await put(n);
      placed += len(n);
      onProgress(placed);
    }
  };
  await Promise.all(Array.from({ length: Math.min(concurrency, todo.length) }, worker));
  const done = await call(base, "complete", { ...head, upload_id: st.upload_id }, signal);
  return { key: done.key, sent: todo.length, skipped: have.size, already: false,
           camera: done.camera, duration_s: done.duration_s, indexed: done.indexed };
}
