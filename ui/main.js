// Outil de tri des balades Insta360 X5 : lecture des proxys .lrv (double fisheye),
// recadrage 360 en WebGL, frise des métriques IMU/GPS, carte, sélection de clips, montage.
//
// Point d'entrée (chargé en module par index.html) : importe les modules de l'interface
// (chacun branche ses propres commandes en s'évaluant), puis charge la première session
// et lance la boucle d'affichage.
//
// Carte des modules (tous dans ui/, servis à plat sous /ui/) :
//   state.js          état partagé `st`, constantes, tête de lecture
//   util.js           $, appels serveur, formats, stockage local
//   geometry.js       horizon, points clés, projection des zones (= core/src/geometry.rs)
//   view.js           cadrage affiché, suivi des points clés, horizon mesuré
//   playback.js       lecture multi-segments, vitesses
//   viewer.js         rendu WebGL et gestes sur l'image
//   zones.js          zones floutées sur l'aperçu, tracé à la main, suivi d'un compagnon
//   settings.js       masques fixes et incrustations (réglages serveur)
//   timeline.js       frise
//   map.js            carte Leaflet
//   clips.js          clips de la session, liste
//   clip-editor.js    éditeur du clip sélectionné, points clés
//   stats.js          onglet Stats
//   session.js        chargement d'une session, synchro GPS
//   export.js         exports de la session, résumé hyperlapse
//   project.js        onglet Fichiers, dossiers de vidéos
//   panels.js         panneaux redimensionnables, tiroir mobile, onglets du projet
//   montage.js        onglet Montage : ordre, montage auto, export
//   finish.js         finition du montage
//   music-library.js  bibliothèque de musiques libres
//   privacy.js        volet Confidentialité
//   commands.js       barre de commandes, menus, aide, raccourcis clavier

import { st, video, now, series } from "./state.js";
import { $, $$, fmt, localClock } from "./util.js";
import { clipKeys, clipMode } from "./geometry.js";
import { activeClip, followsKeyframes, setView } from "./view.js";
import { applyRate, seek, setRate } from "./playback.js";
import { render } from "./viewer.js";
import { loadSettings } from "./settings.js";
import { drawTimeline, followTimeline } from "./timeline.js";
import { updateMapPos } from "./map.js";
import { loadSession } from "./session.js";
import { refreshSessions } from "./project.js";
import { pollMontage } from "./montage.js";
// modules qui ne font que brancher leurs commandes
import "./stats.js";
import "./export.js";
import "./panels.js";
import "./finish.js";
import "./music-library.js";
import "./privacy.js";
import "./commands.js";

// ------------------------------------------------------------------ boucle d'affichage

/** Texte du HUD : heure, vitesse, intérêt, horizon et état du suivi des points clés. */
function hudText(t, ac) {
  const sp = series("speed", t);
  let txt = `${localClock(t)} · ${sp !== null ? sp.toFixed(0) + " km/h" : "— km/h"} · intérêt ${(series("score", t) * 100).toFixed(0)}`;
  const mode = ac ? clipMode(ac) : st.view.horizon;
  txt += mode === "auto" ? ` · ${st.horizonStatus || "horizon : auto"}` : ` · horizon : ${mode}`;
  if (ac && clipKeys(ac).length) {
    const onKey = clipKeys(ac).some((k) => Math.abs(k.t - (t - ac.start)) < 0.1);
    txt += onKey ? " · ◆ sur un point : tout recadrage le modifie"
      : followsKeyframes() ? " · ◆ suit les points clés" : " · ◆ recadrage en cours (K pour mémoriser)";
  }
  return txt;
}

let lastClipRender = 0;
function frame(ts) {
  if (st.s) {
    const t = now();
    render();
    drawTimeline();
    applyRate();
    if (!video.paused) followTimeline(t);
    const sc = st.clips[st.sel];
    if (st.loop && sc && !video.paused && (t > sc.end || t < sc.start - 1)) seek(sc.start, true);   // boucle sur le clip
    updateMapPos(t);
    $("#clock").textContent = localClock(t);
    $("#tpos").textContent = `${fmt(t)} / ${fmt(st.s.duration)}`;
    if (ts - lastClipRender > 500) {   // clip en cours de lecture surligné dans la liste
      lastClipRender = ts;
      $$("#clips li").forEach((li, k) => li.classList.toggle("current", t >= st.clips[k]?.start && t <= st.clips[k]?.end));
    }
    const ac = activeClip(t);
    $("#hud").textContent = hudText(t, ac);
    if (ac !== st.lastActive) { st.lastActive = ac; st.viewOverride = false; }   // entrée dans un clip : on suit ses points
    if (!st.viewOverride) setView({});
  }
  requestAnimationFrame(frame);
}

// ------------------------------------------------------------------ démarrage

(async function init() {
  setRate(1);
  setView({});
  await loadSettings();
  const sessions = await refreshSessions();
  const wanted = location.hash.slice(1);
  const first = sessions.find((s) => s.id === wanted) || sessions.reduce((a, b) => (b.duration > a.duration ? b : a));
  await loadSession(first.id);
  pollMontage();
  requestAnimationFrame(frame);
})();
