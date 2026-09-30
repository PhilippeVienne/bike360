// Lecture multi-segments : une session est une suite de fichiers .lrv (enregistrement en
// boucle) lus un par un dans l'élément <video> caché ; la visionneuse WebGL en fait la texture.
// Partage : seek, togglePlay, applyRate, setRate, jumpCandidate.

import { st, video, now, series, SKIM_THRESHOLD } from "./state.js";
import { $, $$ } from "./util.js";
import { followTimeline } from "./timeline.js";

function segmentAt(t) {
  const segs = st.s.segments;
  for (let i = segs.length - 1; i >= 0; i--) if (t >= segs[i].offset) return i;
  return 0;
}

/** Place la tête de lecture à t (secondes de session), en changeant de segment si besoin.
 *  play : true = lancer, false = rester en pause, absent = garder l'état actuel. */
export function seek(t, play) {
  st.viewOverride = false;   // se déplacer = revenir au cadrage des points clés
  t = Math.max(0, Math.min(st.s.duration - 0.1, t));
  const i = segmentAt(t);
  const local = t - st.s.segments[i].offset;
  const wasPlaying = play ?? !video.paused;
  if (i !== st.seg) {
    st.seg = i;
    st.pendingSeek = { local, play: wasPlaying };
    video.src = "/media/" + encodeURIComponent(st.s.segments[i].lrv);
  } else {
    video.currentTime = local;
    if (wasPlaying) video.play();
  }
  followTimeline(t);
}

export function togglePlay() { video.paused ? video.play() : video.pause(); }

/** Vitesse effective : celle choisie, ou 16× sur les passages calmes en mode survol. */
export function applyRate() {
  let r = st.rate;
  if (st.skim && st.s && series("score", now()) < SKIM_THRESHOLD) r = 16;
  if (video.playbackRate !== r) video.playbackRate = r;
}

export function setRate(r) {
  st.rate = r;
  $$("#rates button").forEach((b) => b.classList.toggle("active", +b.dataset.r === r));
  applyRate();
}

/** Saute au moment fort suivant (dir > 0) ou précédent, 5 s avant lui. */
export function jumpCandidate(dir) {
  const t = now();
  const list = dir > 0 ? st.s.candidates.filter((c) => c > t + 1) : st.s.candidates.filter((c) => c < t - 3).reverse();
  if (list.length) seek(Math.max(0, list[0] - 5));
}

// état de la vidéo affiché sur la scène (chargement lent depuis la carte SD, erreur…)
function stageMsg(html, error) {
  const el = $("#stage-msg");
  el.hidden = !html;
  el.classList.toggle("error", !!error);
  if (html) el.innerHTML = html;
}

video.addEventListener("loadedmetadata", () => {
  if (!st.pendingSeek) return;
  video.currentTime = st.pendingSeek.local;
  if (st.pendingSeek.play) video.play();
  st.pendingSeek = null;
  applyRate();
});
video.addEventListener("loadstart", () => stageMsg('<span><span class="spin"></span>Chargement de la vidéo…</span>'));
video.addEventListener("waiting", () => { if (video.readyState < 3) stageMsg('<span><span class="spin"></span>Lecture en attente de données…</span>'); });
for (const ev of ["loadeddata", "canplay", "playing", "seeked"]) video.addEventListener(ev, () => stageMsg(null));
video.addEventListener("error", () => stageMsg(`Vidéo illisible (${video.error?.message || "code " + video.error?.code}).<br>
  La carte SD est-elle toujours montée ?`, true));
video.addEventListener("ended", () => {   // fin d'un segment : on enchaîne sur le suivant
  if (st.seg < st.s.segments.length - 1) seek(st.s.segments[st.seg + 1].offset + 0.01, true);
});
video.addEventListener("play", () => { $("#play").textContent = "❚❚"; if (!st.autoKey) st.viewOverride = false; });
video.addEventListener("pause", () => ($("#play").textContent = "▶"));
