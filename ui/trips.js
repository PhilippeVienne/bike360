// Page des trajets GeoRide : liste des balades vues par le tracker (avec ou sans caméra),
// synchronisées depuis l'API GeoRide (voir core/src/georide.rs et la route POST
// /api/georide/sync côté serveur). Un seul tracker supporté (comme le reste de l'intégration
// GeoRide de Bike360).

const $ = (s) => document.querySelector(s);

const map = L.map("trips-map", { zoomControl: true, attributionControl: true });
L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", { maxZoom: 18, attribution: "© OpenStreetMap" }).addTo(map);
map.setView([45.76, 4.84], 9);
const tripLayer = L.layerGroup().addTo(map);

let trips = [];

function fmtDate(iso) {
  if (!iso) return "—";
  const d = new Date(iso);
  return isNaN(d) ? iso : d.toLocaleString("fr-FR", { dateStyle: "medium", timeStyle: "short" });
}

function renderList() {
  const ul = $("#trip-list");
  if (!trips.length) {
    ul.innerHTML = `<li class="muted">Aucun trajet synchronisé. Clique « Synchroniser ».</li>`;
    return;
  }
  ul.innerHTML = trips.map((t, i) => `
    <li class="trip-row" data-i="${i}">
      <span class="when">${fmtDate(t.started_at)}</span>
      <span class="grow">${t.distance_km != null ? t.distance_km.toFixed(1) + " km" : "distance inconnue"}</span>
      <span class="muted">${t.duration_min != null ? Math.round(t.duration_min) + " min" : ""}</span>
    </li>`).join("");
  ul.querySelectorAll(".trip-row").forEach((li) => li.addEventListener("click", () => selectTrip(Number(li.dataset.i))));
  if (trips.length) selectTrip(0);
}

function selectTrip(i) {
  document.querySelectorAll(".trip-row").forEach((li, k) => li.classList.toggle("sel", k === i));
  tripLayer.clearLayers();
  const t = trips[i];
  const path = (t.path || []).filter((p) => Array.isArray(p) && p.length === 2);
  if (!path.length) return;
  L.polyline(path, { color: "#f5a524", weight: 4, opacity: 0.9 }).addTo(tripLayer);
  L.circleMarker(path[0], { radius: 6, color: "#fff", fillColor: "#22c55e", fillOpacity: 1 }).bindTooltip("départ").addTo(tripLayer);
  L.circleMarker(path[path.length - 1], { radius: 6, color: "#fff", fillColor: "#ef4444", fillOpacity: 1 }).bindTooltip("arrivée").addTo(tripLayer);
  map.fitBounds(L.latLngBounds(path), { padding: [20, 20] });
}

async function load() {
  const r = await fetch("/api/georide/trips");
  const j = await r.json().catch(() => ({ trips: [] }));
  trips = (j.trips || []).slice();
  $("#last-sync").textContent = j.last_sync ? `Dernière synchro : ${fmtDate(j.last_sync)}` : "Jamais synchronisé";
  renderList();
}

$("#sync").addEventListener("click", async () => {
  $("#sync").disabled = true;
  $("#message").textContent = "Synchronisation avec GeoRide…";
  try {
    const r = await fetch("/api/georide/sync", { method: "POST" });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) { $("#message").textContent = j.error || "Synchronisation impossible."; return; }
    $("#message").textContent = `${j.synced} nouveau(x) trajet(s) (${j.total} au total).`;
    await load();
  } catch {
    $("#message").textContent = "Serveur injoignable.";
  } finally {
    $("#sync").disabled = false;
  }
});

load();
