// Page d'envoi des rushs (module séparé de l'atelier) : on choisit le dossier DCIM de la carte,
// la page liste les balades trouvées, puis envoie les aperçus d'abord et les originaux ensuite.
// Refermer l'onglet interrompt l'envoi ; rechoisir le même dossier le reprend là où il en était.

import { RUSH_RE, sessionsOf, queueOf, sendFile } from "./envoi-core.js";

const $ = (s) => document.querySelector(s);
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const size = (b) => b >= 1e9 ? `${(b / 1e9).toFixed(1).replace(".", ",")} Go` : `${Math.max(1, Math.round(b / 1e6))} Mo`;
const clock = (s) => { s = Math.round(s); return s >= 3600 ? `${Math.floor(s / 3600)} h ${String(Math.floor(s % 3600 / 60)).padStart(2, "0")}` : `${Math.floor(s / 60)} min ${String(s % 60).padStart(2, "0")} s`; };

let sessions = [];
let picked = false;   // un dossier a été choisi (sinon rien à dire sur son contenu)
let abort = null;     // envoi en cours : AbortController

function render() {
  const total = sessions.reduce((a, s) => a + s.bytes, 0);
  $("#found").innerHTML = sessions.length
    ? `<p><strong>${sessions.length} session${sessions.length > 1 ? "s" : ""}</strong> · ${size(total)}</p>
       <ul class="src-list">${sessions.map((s) => `<li><span class="path">${s.date.slice(6)}/${s.date.slice(4, 6)}/${s.date.slice(0, 4)} · ${s.time.slice(0, 2)}h${s.time.slice(2, 4)}</span>
         <span class="muted">${s.previews.length} aperçu${s.previews.length > 1 ? "s" : ""}, ${s.originals.length} original${s.originals.length > 1 ? "aux" : ""} · ${size(s.bytes)}</span></li>`).join("")}</ul>`
    : picked ? `<p class="muted">Aucune vidéo de la caméra dans ce dossier (fichiers VID_….insv et LRV_….lrv attendus).</p>` : "";
  $("#send").disabled = !sessions.length || !!abort;
  $("#stop").hidden = !abort;
}

$("#pick").addEventListener("change", (e) => {
  sessions = sessionsOf([...e.target.files].filter((f) => RUSH_RE.test(f.name)));
  picked = true;
  $("#status").textContent = "";
  render();
});

$("#send").addEventListener("click", async () => {
  const queue = queueOf(sessions, { originals: $("#originals").checked });
  const total = queue.reduce((a, f) => a + f.size, 0);
  const bar = $("#progress"), status = $("#status");
  let before = 0, sent = 0, t0 = performance.now(), first = null;
  abort = new AbortController();
  bar.hidden = false;
  render();
  try {
    for (const [i, f] of queue.entries()) {
      await sendFile(f, {
        signal: abort.signal,
        onProgress: (placed) => {
          const done = before + placed;
          first ??= done;                       // ce qui était déjà en place ne compte pas dans le débit
          const rate = (done - first) / ((performance.now() - t0) / 1000);
          bar.value = done / total;
          status.textContent = `${i + 1}/${queue.length} · ${f.name} · ${size(done)} sur ${size(total)}` +
            (rate > 0 && done < total ? ` · reste ${clock((total - done) / rate)}` : "");
        },
      });
      before += f.size;
      sent++;
    }
    status.textContent = `✓ ${sent} fichier${sent > 1 ? "s" : ""} en place (${size(total)}).`;
  } catch (e) {
    status.textContent = abort.signal.aborted
      ? `Envoi interrompu après ${sent} fichier${sent > 1 ? "s" : ""}. Relancer reprend où il en était.`
      : `⚠ ${esc(e.message)} — relancer reprend où il en était.`;
  }
  abort = null;
  render();
});

$("#stop").addEventListener("click", () => abort && abort.abort());
// fermer l'onglet interrompt l'envoi : le navigateur demande confirmation
addEventListener("beforeunload", (e) => { if (abort) e.preventDefault(); });
render();
