// État partagé de l'interface et constantes.
// Module « feuille » (aucun import) : tous les autres modules lisent et modifient l'objet `st`
// (jamais réaffecté, seulement ses champs), ce qui évite de réaffecter une liaison importée.

export const LENS_FOV = 195;              // champ d'un objectif (°), doit correspondre au serveur
export const RATES = [1, 2, 4, 8, 16];    // vitesses de lecture (touches 1 à 5)
export const SKIM_THRESHOLD = 0.45;       // score sous lequel le survol accélère
export const QUICK_CLIP = [5, 10];        // « + Clip » : 5 s avant, 10 s après la tête de lecture
export const DEFAULT_VIEW = { yaw: 0, pitch: -10, fov: 100, roll: 0 };   // vue avant d'une caméra tournée vers l'avant
/** Vue avant de la session affichée (selon la position de la caméra), aussi pour la vue brute. */
export const defaultView = () => ({ ...DEFAULT_VIEW, yaw: (st.s && st.s.front_yaw) || 0 });

export const st = {
  // session affichée
  s: null,                        // session courante (JSON d'analyse)
  seg: -1,                        // index du segment chargé dans <video>
  pendingSeek: null,              // position à atteindre une fois le segment chargé
  horizon: null,                  // horizon mesuré dans l'image ({up, hz}) quand l'analyse est finie
  horizonStatus: "",              // état de l'analyse d'horizon (affiché dans le HUD)
  pvTracks: {},                   // zones de confidentialité par clip (aperçu sur la vidéo)

  // clips de la session
  clips: [],
  sel: null,                      // index du clip sélectionné (éditeur)
  checked: new Set(),             // clips cochés (suppression en masse)
  loop: false,                    // lecture en boucle du clip sélectionné
  inPoint: null,                  // début posé (I) du clip en cours de création

  // lecture
  rate: 1,
  skim: false,                    // survol : accélère les passages calmes

  // cadrage
  view: { ...DEFAULT_VIEW, raw: false, horizon: "fixe" },
  viewOverride: false,            // vue modifiée à la main : ne suit plus les points clés
  autoKey: false,                 // « Auto » : chaque changement de vue enregistre un point clé
  dragging: false,                // glisser en cours sur l'image
  lastActive: null,               // clip traversé à l'image précédente

  // tracés sur l'image
  masks: [],                      // zones floutées fixes {x,y,w,h} en coordonnées du .lrv (0..1)
  maskEdit: false,
  maskDraft: null,
  zoneEdit: false,                // tracé d'un rectangle sur la vue (zone à flouter ou compagnon à suivre)
  zoneMode: "privacy",            // « privacy » ou « follow »
  zoneDraft: null,
  telemetry: {},                  // incrustations cochées (réglages serveur)

  // frise
  tl: { v0: 0, v1: 1 },           // fenêtre visible de la frise (secondes)
  hover: null,                    // instant survolé sur la frise

  // projet
  sessions: [],                   // toutes les sessions connues du serveur
  project: { sessions: [], order: [], excluded: [], clips: [] },
};

export const video = document.querySelector("#video");

/** Instant de la tête de lecture dans la session (secondes depuis le début du premier segment). */
export function now() {
  if (!st.s || st.seg < 0) return 0;
  return st.s.segments[st.seg].offset + video.currentTime;
}

/** Valeur d'une série à la seconde t (bornée à la durée de la session). */
export const series = (k, t) => st.s.series[k][Math.max(0, Math.min(st.s.duration - 1, Math.floor(t)))];
