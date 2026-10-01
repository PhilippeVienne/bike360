// Carte Leaflet (L est chargé en script classique par index.html) : trace GPS colorée par
// l'intérêt, moments forts, clips en vert, position de la tête de lecture ; un clic sur la
// carte va au point le plus proche de la trace.
// Partage : drawMap, drawClipsOnMap, updateMapPos, refreshMapSize.

import { st } from "./state.js";
import { $, localClock } from "./util.js";
import { seek } from "./playback.js";
import { onStep, onTab } from "./steps.js";

const map = L.map("map", { zoomControl: true, attributionControl: true });
new ResizeObserver(() => refreshMapSize()).observe($("#map"));
// La carte peut être masquée (autre étape, autre onglet) quand la trace est chargée : elle
// est alors recadrée dès qu'elle redevient visible.
let pendingFit = null;
L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", {
  maxZoom: 18, attribution: "© OpenStreetMap",
}).addTo(map);
map.setView([45.76, 4.84], 9);
const mapLayers = L.layerGroup().addTo(map), clipLayer = L.layerGroup().addTo(map);
const posMarker = L.circleMarker([0, 0], { radius: 7, color: "#fff", weight: 2, fillColor: "#ff4d4f", fillOpacity: 1 });

/** À appeler quand le conteneur de la carte change de taille ou redevient visible. */
export function refreshMapSize() {
  map.invalidateSize();
  if (pendingFit && $("#map").clientWidth > 0) { map.fitBounds(pendingFit, { padding: [20, 20] }); pendingFit = null; }
}
onTab((g, name) => { if (name === "map") refreshMapSize(); });
onStep(() => refreshMapSize());

function heatColor(v) {
  // bleu (calme) → ambre (intéressant)
  const a = Math.max(0, Math.min(1, v ?? 0));
  const r = Math.round(90 + a * (245 - 90)), g = Math.round(169 + a * (165 - 169)), b = Math.round(255 + a * (36 - 255));
  return `rgb(${r},${g},${b})`;
}

/** Trace de la session (un point toutes les 5 s) et moments forts ; cadre la carte dessus. */
export function drawMap() {
  mapLayers.clearLayers();
  const { lat, lon, score } = st.s.series;
  const pts = [];
  for (let i = 0; i < lat.length; i += 5) {
    if (lat[i] === null) continue;
    pts.push([lat[i], lon[i], i]);
  }
  if (!pts.length) { posMarker.remove(); pendingFit = null; return; }
  for (let k = 1; k < pts.length; k++) {
    const [a, b] = [pts[k - 1], pts[k]];
    if (b[2] - a[2] > 30) continue;   // trou GPS : pas de trait
    L.polyline([[a[0], a[1]], [b[0], b[1]]], { color: heatColor(score[b[2]]), weight: 4, opacity: 0.9 }).addTo(mapLayers);
  }
  st.s.candidates.forEach((t, k) => {
    if (lat[t] === null) return;
    L.circleMarker([lat[t], lon[t]], { radius: 5, color: "#f5a524", fillOpacity: 1 })
      .bindTooltip(`candidat ${k + 1} · ${localClock(t)}`).on("click", () => seek(t)).addTo(mapLayers);
  });
  posMarker.addTo(map);
  const bounds = L.latLngBounds(pts.map((p) => [p[0], p[1]]));
  if ($("#map").clientWidth > 0) { map.fitBounds(bounds, { padding: [20, 20] }); pendingFit = null; }
  else pendingFit = bounds;
  map.off("click").on("click", (e) => {
    let best = null, bd = Infinity;
    for (const [la, lo, i] of pts) {
      const d = (la - e.latlng.lat) ** 2 + (lo - e.latlng.lng) ** 2;
      if (d < bd) { bd = d; best = i; }
    }
    if (best !== null) seek(best);
  });
}

export function drawClipsOnMap() {
  clipLayer.clearLayers();
  const { lat, lon } = st.s.series;
  for (const c of st.clips) {
    const line = [];
    for (let i = Math.floor(c.start); i <= Math.ceil(c.end) && i < lat.length; i++) if (lat[i] !== null) line.push([lat[i], lon[i]]);
    if (line.length > 1) L.polyline(line, { color: "#3ecf8e", weight: 8, opacity: 0.7 }).addTo(clipLayer);
  }
}

export function updateMapPos(t) {
  const { lat, lon } = st.s.series;
  const i = Math.floor(t);
  if (lat[i] != null) posMarker.setLatLng([lat[i], lon[i]]);
}
