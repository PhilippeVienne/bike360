// Page pit-stop : station-service la moins chère le long de la route (voir core/src/pitstop.rs
// pour le calcul — France sans clé, Allemagne si TANKERKOENIG_API_KEY est configurée côté
// serveur). Position de départ : géolocalisation du téléphone, sinon saisie à la main.

const $ = (s) => document.querySelector(s);

const map = L.map("ps-map", { zoomControl: true, attributionControl: true });
L.tileLayer("https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png", { maxZoom: 18, attribution: "© OpenStreetMap" }).addTo(map);
map.setView([45.76, 4.84], 9);
const markers = L.layerGroup().addTo(map);
let originMarker = null;

function setOrigin(lat, lon) {
  $("#lat").value = lat.toFixed(5);
  $("#lon").value = lon.toFixed(5);
  if (originMarker) originMarker.remove();
  originMarker = L.circleMarker([lat, lon], { radius: 7, color: "#fff", weight: 2, fillColor: "#3b82f6", fillOpacity: 1 })
    .bindTooltip("départ").addTo(map);
  map.setView([lat, lon], 11);
}

$("#locate").addEventListener("click", () => {
  if (!navigator.geolocation) { $("#message").textContent = "Géolocalisation indisponible sur cet appareil."; return; }
  $("#message").textContent = "Localisation…";
  navigator.geolocation.getCurrentPosition(
    (pos) => { setOrigin(pos.coords.latitude, pos.coords.longitude); $("#message").textContent = ""; },
    () => { $("#message").textContent = "Position refusée : saisis latitude/longitude à la main."; },
    { enableHighAccuracy: true, timeout: 10000 }
  );
});

function priceLabel(r) {
  if (!r.has_price) return "prix inconnu";
  return `${r.price_l.toFixed(3)} €/L`;
}

function renderResults(results) {
  const ul = $("#results");
  markers.clearLayers();
  if (!results.length) {
    ul.innerHTML = `<li class="muted">Aucune station trouvée dans ce rayon.</li>`;
    return;
  }
  ul.innerHTML = results.map((r, i) => `
    <li class="ps-row ${r.has_price ? "" : "no-price"}">
      <span class="rank">${i + 1}</span>
      <span class="name">${r.brand || r.name || "Station"} <span class="muted">${r.city || ""}</span></span>
      <span class="price">${priceLabel(r)}</span>
      <span class="muted">${r.distance_km.toFixed(1)} km</span>
      ${r.has_price ? `<span class="muted">total ${r.total_cost_eur.toFixed(2)} €</span>` : ""}
    </li>`).join("");
  results.forEach((r, i) => {
    L.circleMarker([r.latitude, r.longitude], {
      radius: 6, color: "#fff", weight: 1.5,
      fillColor: i === 0 && r.has_price ? "#22c55e" : r.has_price ? "#f5a524" : "#6b7280",
      fillOpacity: 1,
    }).bindTooltip(`${r.brand || r.name || "Station"} · ${priceLabel(r)}`).addTo(markers);
  });
  const bounds = L.latLngBounds(results.map((r) => [r.latitude, r.longitude]));
  if (originMarker) bounds.extend(originMarker.getLatLng());
  map.fitBounds(bounds, { padding: [24, 24] });
}

$("#form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const lat = parseFloat($("#lat").value), lon = parseFloat($("#lon").value);
  if (Number.isNaN(lat) || Number.isNaN(lon)) { $("#message").textContent = "Latitude et longitude requises."; return; }
  const qs = new URLSearchParams({
    lat, lon,
    radius: $("#radius").value || "20",
    fuel: $("#fuel").value,
    fill: $("#fill").value || "12",
    consumption: $("#consumption").value || "5",
    exclude_distance: $("#exclude_distance").checked ? "1" : "0",
  });
  $("#message").textContent = "Recherche…";
  $("#search").disabled = true;
  try {
    const r = await fetch(`/api/pitstops?${qs}`);
    const j = await r.json();
    if (!r.ok) { $("#message").textContent = j.error || "Erreur de recherche."; return; }
    $("#message").textContent = `${j.results.length} station(s) trouvée(s).`;
    renderResults(j.results);
  } catch {
    $("#message").textContent = "Serveur injoignable.";
  } finally {
    $("#search").disabled = false;
  }
});

// Première ouverture : tente la géolocalisation automatiquement.
if (navigator.geolocation) {
  navigator.geolocation.getCurrentPosition(
    (pos) => setOrigin(pos.coords.latitude, pos.coords.longitude),
    () => {},
    { timeout: 5000 }
  );
}
