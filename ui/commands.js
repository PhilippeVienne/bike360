// Commandes : barre sous la vidéo (lecture, vitesses, moments forts, points clés, vues),
// menus déroulants, dialogues et feuilles ([data-open="id"] ouvre <dialog id>), aide et
// raccourcis clavier (ordinateur ; tout reste faisable au doigt sans eux).

import { st, video, now, RATES, DEFAULT_VIEW } from "./state.js";
import { $, $$ } from "./util.js";
import { activeClip, setView, upsertKey, userView } from "./view.js";
import { applyRate, jumpCandidate, seek, setRate, togglePlay } from "./playback.js";
import { deleteSelected, editEdge, markIn, markOut, quickClip, updateMarkUI } from "./clips.js";
import { jumpKey, renderEditor } from "./clip-editor.js";
import { showWholeSession } from "./timeline.js";
import { setZoneEdit } from "./zones.js";
import { onStep } from "./steps.js";

// ------------------------------------------------------------------ barre de commandes

RATES.forEach((r) => {
  const b = document.createElement("button");
  b.textContent = r + "×"; b.dataset.r = r;
  b.addEventListener("click", () => setRate(r));
  $("#rates").appendChild(b);
});
$("#play").addEventListener("click", togglePlay);
$("#skim").addEventListener("change", (e) => { st.skim = e.target.checked; applyRate(); });
$("#view-front").addEventListener("click", () => userView({ ...DEFAULT_VIEW, raw: false }));
$("#view-rider").addEventListener("click", () => userView({ yaw: 180, pitch: 8, fov: 100, roll: 0, raw: false }));
$("#horizon-mode").addEventListener("change", (e) => setView({ horizon: e.target.value }));
$("#view-raw").addEventListener("click", () => setView({ raw: !st.view.raw }));
$("#view-roll").addEventListener("input", (e) => userView({ roll: +e.target.value }));

const addKeyHere = () => { const c = activeClip(now()); if (c) upsertKey(c, now()); };
$("#key-prev").addEventListener("click", () => jumpKey(-1));
$("#key-next").addEventListener("click", () => jumpKey(1));
$("#key-add").addEventListener("click", addKeyHere);
$("#cand-prev").addEventListener("click", () => jumpCandidate(-1));
$("#cand-next").addEventListener("click", () => jumpCandidate(1));

// étape ① : pas de vidéo affichée, on la met en pause
onStep((s) => { if (s === "files") video.pause(); });

// ------------------------------------------------------------------ menus et aide

// menus déroulants : un seul ouvert, fermés au clic ailleurs ou sur Échap
document.addEventListener("click", (e) => {
  $$("details.menu[open]").forEach((d) => { if (!d.contains(e.target)) d.open = false; });
});
$$("details.menu").forEach((d) => d.addEventListener("toggle", () => {
  if (d.open) $$("details.menu[open]").forEach((o) => { if (o !== d) o.open = false; });
}));
// dialogues et feuilles : [data-open="id"] ouvre, clic sur le fond ou [data-close] ferme ;
// l'événement « open-dialog » permet au module concerné de rafraîchir son contenu.
export function openDialog(id, section) {
  const dlg = $("#" + id);
  if (!dlg) return;
  $$("dialog[open]").forEach((d) => d !== dlg && d.close());
  if (!dlg.open) dlg.showModal();
  dlg.dispatchEvent(new Event("open-dialog"));
  if (section) dlg.querySelector(`[data-section="${section}"]`)?.scrollIntoView({ block: "start" });
}
document.addEventListener("click", (e) => {
  const o = e.target.closest("[data-open]");
  if (o) { e.preventDefault(); openDialog(o.dataset.open, o.dataset.section); return; }
  const dlg = e.target.closest("dialog");
  if (dlg && dlg.id !== "lib" && (e.target === dlg || e.target.closest("[data-close]"))) dlg.close();
});
const helpDlg = $("#help");
$("#help-open").addEventListener("click", () => openDialog("help"));

// ------------------------------------------------------------------ raccourcis clavier (liste dans l'aide, index.html)

document.addEventListener("keydown", (e) => {
  if (e.key === "?" && !helpDlg.open) { openDialog("help"); e.preventDefault(); return; }
  if (e.key === "Escape") $$("details.menu[open]").forEach((d) => (d.open = false));
  // pas de raccourci pendant une saisie (sauf cases à cocher et curseurs) ni avec Ctrl/Alt/⌘
  if (!st.s || $("dialog[open]") || e.target.tagName === "SELECT" || e.target.tagName === "INPUT" && e.target.type !== "checkbox" && e.target.type !== "range"
      || e.ctrlKey || e.metaKey || e.altKey) return;
  const k = e.key.toLowerCase();
  const big = e.shiftKey ? 30 : 5;
  const actions = {
    " ": togglePlay, arrowleft: () => seek(now() - big), arrowright: () => seek(now() + big),
    i: markIn, o: markOut, c: quickClip, n: () => jumpCandidate(1), p: () => jumpCandidate(-1),
    "[": () => editEdge("start", now()), "]": () => editEdge("end", now()),
    k: addKeyHere,
    ",": () => jumpKey(-1), ".": () => jumpKey(1),
    delete: deleteSelected, l: () => { st.loop = !st.loop; renderEditor(); },
    h: () => setView({ horizon: { auto: "fixe", fixe: "aucun", aucun: "auto" }[st.view.horizon] }),
    f: () => $("#view-front").click(), r: () => $("#view-rider").click(), v: () => $("#view-raw").click(),
    m: () => $("#skim").click(), 0: showWholeSession,
    escape: () => { st.inPoint = null; updateMarkUI(); if (st.zoneEdit) setZoneEdit(false); },
  };
  if (/^[1-5]$/.test(e.key)) { setRate(RATES[+e.key - 1]); e.preventDefault(); return; }
  if (actions[k]) { actions[k](); e.preventDefault(); }
});
