// Onglets Carte / Stats du panneau de droite, et tuiles de statistiques (session, montage).
// Partage : renderStats.

import { st } from "./state.js";
import { $, $$, fmt } from "./util.js";
import { refreshMapSize } from "./map.js";

$$("[data-tab]").forEach((b) => b.addEventListener("click", () => {
  $$("[data-tab]").forEach((x) => x.classList.toggle("active", x === b));
  $("#map").hidden = b.dataset.tab !== "map";
  $("#stats").hidden = b.dataset.tab !== "stats";
  if (b.dataset.tab === "map") refreshMapSize();
}));

export function renderStats() {
  const s = st.s?.stats, el = $("#stats");
  if (!s) return;
  const tile = (v, l) => `<div class="stat"><div class="v">${v}</div><div class="l">${l}</div></div>`;
  const clipTime = st.clips.reduce((a, c) => a + c.end - c.start, 0);
  let clipKm = 0;   // distance parcourue pendant les clips (GPS, seconde par seconde)
  const { lat, lon } = st.s.series;
  for (const c of st.clips) for (let i = Math.ceil(c.start); i < Math.floor(c.end); i++) {
    if (lat[i] == null || lat[i + 1] == null) continue;
    const dy = (lat[i + 1] - lat[i]) * 111.2, dx = (lon[i + 1] - lon[i]) * 111.2 * Math.cos(lat[i] * Math.PI / 180);
    clipKm += Math.hypot(dx, dy);
  }
  let html = `<div class="stat-section">Session</div><div class="stat-grid">
    ${tile(fmt(s.duration_s), "durée filmée")}`;
  if (s.distance_km !== undefined) {
    html += `${tile(s.distance_km + " km", "distance")}
      ${tile(s.avg_speed_kmh + " km/h", "moyenne en roulant")}
      ${tile(s.max_speed_kmh + " km/h", "vitesse max")}
      ${tile(`${s.alt_min_m}–${s.alt_max_m} m`, "altitude")}
      ${tile(`+${s.climb_m} / −${s.descent_m} m`, "dénivelé (≈, GPS)")}
      ${tile(`${st.s.tilt.pitch.toFixed(0)}° / ${st.s.tilt.roll.toFixed(0)}°`, "inclinaison caméra (pitch / roll)")}</div>`;
  } else html += `</div><p class="muted">Pas de GPS pour cette session : stats de trajet indisponibles.</p>`;
  html += `<div class="stat-section">Montage</div><div class="stat-grid">
    ${tile(st.clips.length, "clips")}
    ${tile(fmt(clipTime), "durée du montage")}
    ${tile(clipKm.toFixed(1) + " km", "distance couverte")}
    ${tile(s.duration_s ? Math.round(clipTime / s.duration_s * 100) + " %" : "–", "de la session gardée")}</div>`;
  el.innerHTML = html;
}
