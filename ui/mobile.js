// Entrée de l'interface téléphone (mobile.html) : mise en page propre (plein écran de la
// vidéo, défilement vers l'éditeur), puis démarrage commun (app.js). Toute la logique est
// partagée avec l'interface ordinateur (main.js).

import { $ } from "./util.js";
import { onStep } from "./steps.js";
import { start } from "./app.js";

// ------------------------------------------------------------------ plein écran de la vidéo
// La vue occupe tout l'écran (classe fs) ; on demande aussi le vrai plein écran et le
// paysage quand le navigateur l'accepte (pas sur iPhone : la classe suffit).
const fsBtn = $("#fs-toggle");
async function setFullscreen(on) {
  document.body.classList.toggle("fs", on);
  fsBtn.textContent = on ? "✕" : "⛶";
  fsBtn.setAttribute("aria-label", on ? "Quitter le plein écran" : "Plein écran");
  try {
    if (on && !document.fullscreenElement && document.documentElement.requestFullscreen) {
      await document.documentElement.requestFullscreen();
      await screen.orientation?.lock?.("landscape");
    } else if (!on && document.fullscreenElement) await document.exitFullscreen();
  } catch (e) { /* refusé par le navigateur : la vue remplit quand même la page */ }
}
fsBtn.addEventListener("click", () => setFullscreen(!document.body.classList.contains("fs")));
document.addEventListener("fullscreenchange", () => {
  if (!document.fullscreenElement && document.body.classList.contains("fs")) setFullscreen(false);
});

// changement d'étape : on quitte le plein écran et on remonte en haut de la page
onStep(() => {
  if (document.body.classList.contains("fs")) setFullscreen(false);
  $("#m-main").scrollTop = 0;
});

// clip touché dans la liste : l'éditeur (au-dessus de la liste) est ramené à l'écran
$("#clips").addEventListener("click", (e) => {
  if (!e.target.closest("li:not(.empty)") || e.target.classList.contains("chk")) return;
  requestAnimationFrame(() => $("#clip-editor").scrollIntoView({ block: "start", behavior: "smooth" }));
});

start();
