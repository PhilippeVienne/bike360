// Parcours en étapes, commun aux deux interfaces (ordinateur et téléphone) :
//   ① files : Fichiers   ② cut : Repérer & couper   ③ montage : Montage   ④ export : Exporter
// L'étape courante est posée sur <body data-step="…"> ; les blocs de la page portent
// data-steps="cut montage" (étapes où ils sont visibles, cf. style.css). Chaque bouton
// [data-go="étape"] y mène. Le module affiche aussi l'état de chaque étape, la prochaine
// action conseillée et les tâches en cours (export, analyse…).
// Il fournit enfin les onglets génériques : conteneur [data-tabs="groupe"] de boutons
// [data-tab="nom"], panneaux [data-panel="groupe:nom"].
// Module sans dépendance vers les autres modules d'interface : ils s'y abonnent (onStep, onTab).
// Partage : STEPS, goStep, isStep, onStep, updateSteps, setJob, setExported, showTab,
//           tabShown, onTab.

import { st } from "./state.js";
import { $, $$, esc, fmt, storageGet, storageSet } from "./util.js";

export const STEPS = ["files", "cut", "montage", "export"];
export const STEP_LABELS = { files: "Fichiers", cut: "Repérer & couper", montage: "Montage", export: "Exporter" };

const stepListeners = [];
/** fn(étape, précédente) appelée à chaque changement d'étape. */
export const onStep = (fn) => stepListeners.push(fn);
export const isStep = (s) => document.body.dataset.step === s;

/** Affiche l'étape s (mémorisée dans ce navigateur). */
export function goStep(s) {
  if (!STEPS.includes(s)) s = "cut";
  const prev = document.body.dataset.step;
  document.body.dataset.step = s;
  $$(".steps [data-go]").forEach((b) => {
    const on = b.dataset.go === s;
    b.classList.toggle("current", on);
    if (on) b.setAttribute("aria-current", "step"); else b.removeAttribute("aria-current");
  });
  $$("[data-step-title]").forEach((el) => (el.textContent = `${STEPS.indexOf(s) + 1}. ${STEP_LABELS[s]}`));
  storageSet("bike360.step", s);
  updateSteps();
  if (prev !== s) stepListeners.forEach((fn) => fn(s, prev));
}
/** Étape mémorisée dans ce navigateur (ou null). */
export const initialStep = () => storageGet("bike360.step");

document.addEventListener("click", (e) => {
  const b = e.target.closest("[data-go]");
  if (b && !b.disabled) { e.preventDefault(); goStep(b.dataset.go); }
});

// ------------------------------------------------------------------ état des étapes et prochaine action

let exported = null;   // dernier fichier du montage exporté (nom)
export function setExported(name) { exported = name; updateSteps(); }

/** Résumé de chaque étape (sous son nom), coche des étapes faites, prochaine action. */
export function updateSteps() {
  const p = st.project, nS = p.sessions.length;
  const kept = p.clips.filter((c) => !c.excluded), dur = kept.reduce((a, c) => a + c.end - c.start, 0);
  const plural = (n, w) => `${n} ${w}${n > 1 ? "s" : ""}`;
  const status = {
    files: nS ? plural(nS, "session") : "à choisir",
    cut: p.clips.length ? plural(p.clips.length, "clip") : "aucun clip",
    montage: kept.length ? `${kept.length} · ${fmt(dur)}` : "vide",
    export: jobs.montage ? jobs.montage.text : exported ? "fait" : "",
  };
  const done = { files: nS > 0, cut: p.clips.length > 0, montage: kept.length > 0, export: !!exported };
  for (const s of STEPS) {
    $$(`[data-step-status="${s}"]`).forEach((el) => (el.textContent = status[s]));
    $$(`.steps [data-go="${s}"]`).forEach((b) => b.classList.toggle("done", done[s]));
  }
  // prochaine action : [texte, étape suivante, libellé du bouton]
  const step = document.body.dataset.step;
  let next;
  if (step === "files") next = nS ? ["Touche une vignette pour regarder la session, puis repère les bons moments.", "cut", "Repérer & couper →"]
    : ["Coche « dans le projet » sur les sessions de ta balade.", null];
  else if (step === "cut") next = !st.s ? ["Choisis d'abord une session (étape 1).", "files", "← Fichiers"]
    : !st.clips.length ? ["Pose un clip : ⟦ Début puis Fin ⟧, ou ＋15 s. Les ▼ ambre sont les moments forts.", null]
    : [`${plural(st.clips.length, "clip")} dans cette session, ${plural(p.clips.length, "clip")} dans le projet.`, "montage", "Montage →"];
  else if (step === "montage") next = kept.length ? ["Range les clips, choisis la finition, puis exporte.", "export", "Exporter →"]
    : ["Aucun clip : ✨ montage auto, ou reviens repérer des moments.", "cut", "← Repérer"];
  else next = exported ? ["Montage exporté : le lien est ci-dessous.", null]
    : kept.length ? ["Choisis la destination puis « Exporter le montage ».", null] : ["Aucun clip à exporter.", "cut", "← Repérer"];
  const nt = $("#next-text"), nb = $("#next-btn");
  if (nt) nt.textContent = next[0];
  if (nb) { nb.hidden = !next[1]; if (next[1]) { nb.dataset.go = next[1]; nb.textContent = next[2]; } }
  renderJobs();
}

// ------------------------------------------------------------------ tâches en cours (pastille de l'en-tête)

const jobs = {};   // genre → {text, step}
/** Tâche en cours (texte court) ou terminée (text = null) ; un clic sur la pastille mène à son étape. */
export function setJob(kind, text, step = "export") {
  const had = !!jobs[kind];
  if (text) jobs[kind] = { text, step }; else delete jobs[kind];
  if (had !== !!text || text) renderJobs();
  if (kind === "montage" && had !== !!text) updateSteps();
}
function renderJobs() {
  const pill = $("#job-pill");
  if (!pill) return;
  const list = Object.values(jobs);
  pill.hidden = !list.length;
  if (!list.length) return;
  pill.innerHTML = `<span class="spin"></span>${esc(list[0].text)}${list.length > 1 ? ` <span class="muted">+${list.length - 1}</span>` : ""}`;
  pill.dataset.go = list[0].step;
  pill.title = list.map((j) => j.text).join("\n");
}

// ------------------------------------------------------------------ onglets génériques

const tabListeners = [];
/** fn(groupe, nom) appelée quand un onglet est affiché. */
export const onTab = (fn) => tabListeners.push(fn);
export function showTab(group, name) {
  $$(`[data-tabs="${group}"] [data-tab]`).forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
  $$(`[data-panel^="${group}:"]`).forEach((p) => (p.hidden = p.dataset.panel !== `${group}:${name}`));
  storageSet(`bike360.tab.${group}`, name);
  tabListeners.forEach((fn) => fn(group, name));
}
/** Le panneau groupe:nom est-il l'onglet affiché (et son étape visible) ? */
export function tabShown(group, name) {
  const p = $(`[data-panel="${group}:${name}"]`);
  if (!p || p.hidden) return false;
  const host = p.closest("[data-steps]");
  return !host || host.dataset.steps.split(" ").includes(document.body.dataset.step);
}
document.addEventListener("click", (e) => {
  const b = e.target.closest("[data-tabs] [data-tab]");
  if (b) showTab(b.closest("[data-tabs]").dataset.tabs, b.dataset.tab);
});
/** Onglets au chargement : celui mémorisé, sinon le premier de chaque groupe. */
export function initTabs() {
  $$("[data-tabs]").forEach((g) => {
    const name = g.dataset.tabs, saved = storageGet(`bike360.tab.${name}`);
    const btn = (saved && g.querySelector(`[data-tab="${saved}"]`)) || g.querySelector("[data-tab]");
    if (btn) showTab(name, btn.dataset.tab);
  });
}
