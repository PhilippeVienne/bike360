//! Station-service la moins chère le long d'un trajet (porté de nomadride/nomadride-web).
//!
//! Fournisseurs de prix branchés :
//!   - France : jeu de données ouvert (prix-carburants.gouv.fr), pas de clé requise.
//!   - Allemagne : Tankerkoenig, clé lue dans $TANKERKOENIG_API_KEY (sinon ignoré, pas d'erreur).
//! Les autres pays du site d'origine (Autriche, Espagne, Suisse) demandent chacun une API
//! officielle différente et ne sont pas portés ici ; une station OSM y est tout de même
//! trouvable via Overpass mais sans son prix (voir `enrich_brands`).
//!
//! Caractéristiques locales :
//!   - Pas de base de données : le cache des zones Overpass est un fichier JSON par cellule
//!     sous cache()/pitstop/, et le jeu France complet un seul fichier (comme musiclib.rs).
//!   - Pas de cron de préchauffe ni de verrou "en vol" : l'appel est synchrone (spawn_blocking
//!     côté serveur), les miroirs Overpass sont essayés l'un après l'autre (pas de course de
//!     threads) avec un délai court par miroir.

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::paths;

const USER_AGENT: &str = "Bike360-PitStop/1.0 (outil personnel de montage moto)";
const OVERPASS_ENDPOINTS: [&str; 3] =
    ["https://overpass-api.de/api/interpreter", "https://lz4.overpass-api.de/api/interpreter", "https://z.overpass-api.de/api/interpreter"];
const OVERPASS_MAX_AGE_S: u64 = 90 * 24 * 3600; // au-delà : zone considérée comme jamais interrogée
const FRANCE_API_URL: &str = "https://data.economie.gouv.fr/api/explore/v2.1/catalog/datasets/prix-des-carburants-en-france-flux-instantane-v2/exports/json";
const FRANCE_CACHE_TTL_S: u64 = 3600;
const TANKERKOENIG_URL: &str = "https://creativecommons.tankerkoenig.de/json/list.php";
const TANKERKOENIG_TTL_S: u64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fuel {
    Sp95,
    Sp98,
    E10,
    Gazole,
}

impl Fuel {
    fn key(&self) -> &'static str {
        match self {
            Fuel::Sp95 => "sp95",
            Fuel::Sp98 => "sp98",
            Fuel::E10 => "e10",
            Fuel::Gazole => "gazole",
        }
    }
    pub fn parse(s: &str) -> Fuel {
        match s {
            "sp98" => Fuel::Sp98,
            "e10" => Fuel::E10,
            "gazole" | "diesel" => Fuel::Gazole,
            _ => Fuel::Sp95,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Station {
    pub id: String,
    pub country: String, // "FR" | "DE"
    #[serde(default)]
    pub brand: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub post_code: String,
    pub latitude: f64,
    pub longitude: f64,
    pub prices: HashMap<String, f64>, // clé = Fuel::key(), prix en EUR/L
    pub updated_at_s: u64,            // horodatage unix de la dernière mise à jour du prix
}

#[derive(Debug, Clone, Serialize)]
pub struct Settings {
    pub fill_size_l: f64,     // litres à remplir
    pub consumption_l100: f64, // L/100km de la moto
    pub exclude_distance: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Result_ {
    pub id: String,
    pub country: String,
    pub brand: Option<String>,
    pub name: Option<String>,
    pub address: String,
    pub city: String,
    pub post_code: String,
    pub latitude: f64,
    pub longitude: f64,
    pub has_price: bool,
    pub price_l: Option<f64>,
    pub distance_km: f64,
    pub detour_cost_eur: f64,
    pub total_fuel_cost_eur: f64,
    pub total_cost_eur: f64,
    pub freshness_penalty_eur: f64,
}

/// Distance orthodromique en kilomètres (formule de haversine).
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let (dlat, dlon) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let a = (dlat / 2.0).sin().powi(2) + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

fn now_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn cache_dir() -> PathBuf {
    paths::cache().join("pitstop")
}

fn get_bytes(url: &str, timeout_s: u64) -> Result<Vec<u8>> {
    let resp = ureq::get(url).set("User-Agent", USER_AGENT).timeout(Duration::from_secs(timeout_s)).call()?;
    let mut buf = vec![];
    resp.into_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

// --------------------------------------------------------------------- Overpass (OSM)

/// Éléments `amenity=fuel` d'OSM autour d'un point, utilisés pour trouver la marque d'une
/// station et, hors France/Allemagne, au moins sa position. Les miroirs sont essayés l'un
/// après l'autre (5 s chacun) ; le premier qui répond est gardé.
fn query_overpass_fuel(lat: f64, lon: f64, radius_km: f64) -> Result<Vec<Value>> {
    let radius_m = (radius_km * 1000.0).round() as i64;
    let query = format!(
        "[out:json][timeout:5];(node[\"amenity\"=\"fuel\"](around:{radius_m},{lat},{lon});way[\"amenity\"=\"fuel\"](around:{radius_m},{lat},{lon}););out center tags;"
    );
    let mut last_err = None;
    for endpoint in OVERPASS_ENDPOINTS {
        match ureq::post(endpoint)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .set("User-Agent", USER_AGENT)
            .timeout(Duration::from_secs(5))
            .send_string(&format!("data={}", urlencode(&query)))
        {
            Ok(resp) => {
                let v: Value = resp.into_json().context("réponse Overpass invalide")?;
                return Ok(v["elements"].as_array().cloned().unwrap_or_default());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!("tous les miroirs Overpass ont échoué : {:?}", last_err))
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn overpass_cell_key(lat: f64, lon: f64, radius_km: f64) -> String {
    // Cellule ~2 km : arrondi à 0.02° (comme le cache en base de nomadride, moins fin que
    // le besoin réel mais évite un fichier par requête à quelques mètres près).
    format!("{:.2}_{:.2}_{:.0}", (lat / 0.02).round() * 0.02, (lon / 0.02).round() * 0.02, radius_km.ceil())
}

/// Éléments Overpass pour la zone, servis depuis le cache disque si présent (frais ou non) ;
/// rafraîchis en tâche de fond quand le cache a plus de 24h, en bloquant seulement au premier
/// appel pour une zone jamais vue.
fn fuel_elements_around(lat: f64, lon: f64, radius_km: f64) -> Vec<Value> {
    let path = cache_dir().join(format!("{}.json", overpass_cell_key(lat, lon, radius_km)));
    let age_s = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok()).map(|d| d.as_secs());
    if let Some(age) = age_s {
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(elements) = serde_json::from_str::<Vec<Value>>(&data) {
                if age > 15 * 60 && age <= OVERPASS_MAX_AGE_S {
                    // rafraîchissement en tâche de fond, ne bloque pas la réponse en cours
                    let path2 = path.clone();
                    std::thread::spawn(move || {
                        if let Ok(fresh) = query_overpass_fuel(lat, lon, radius_km) {
                            let _ = std::fs::create_dir_all(cache_dir());
                            let _ = std::fs::write(&path2, serde_json::to_vec(&fresh).unwrap_or_default());
                        }
                    });
                }
                return elements;
            }
        }
    }
    match query_overpass_fuel(lat, lon, radius_km) {
        Ok(elements) => {
            let _ = std::fs::create_dir_all(cache_dir());
            let _ = std::fs::write(&path, serde_json::to_vec(&elements).unwrap_or_default());
            elements
        }
        Err(e) => {
            eprintln!("Pitstop : Overpass indisponible ({e:#}), pas d'enrichissement de marque");
            vec![]
        }
    }
}

/// Complète la marque des stations qui n'en ont pas via Overpass (identifiant national
/// `ref:*` si trouvé, sinon la station OSM la plus proche à moins de 150 m).
fn enrich_brands(stations: &mut [Station], lat: f64, lon: f64, radius_km: f64) {
    if stations.iter().all(|s| s.brand.is_some()) {
        return;
    }
    let elements = fuel_elements_around(lat, lon, radius_km);
    for station in stations.iter_mut() {
        if station.brand.is_some() {
            continue;
        }
        let tags_of = |el: &Value| el.get("tags").cloned().unwrap_or(Value::Null);
        let by_ref = elements.iter().find(|el| {
            tags_of(el).as_object().is_some_and(|t| {
                t.iter().any(|(k, v)| k.starts_with("ref:") && v.as_str().map(str::trim) == Some(station.id.trim()))
            })
        });
        let best = by_ref.or_else(|| {
            elements.iter()
                .filter_map(|el| {
                    let elat = el.get("lat").and_then(Value::as_f64).or_else(|| el["center"]["lat"].as_f64())?;
                    let elon = el.get("lon").and_then(Value::as_f64).or_else(|| el["center"]["lon"].as_f64())?;
                    let d = haversine_km(station.latitude, station.longitude, elat, elon);
                    (d < 0.150).then_some((d, el))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, el)| el)
        });
        if let Some(el) = best {
            let tags = tags_of(el);
            let brand = tags.get("brand").or_else(|| tags.get("operator")).or_else(|| tags.get("name")).and_then(Value::as_str);
            if let Some(b) = brand.map(str::trim).filter(|b| !b.is_empty()) {
                station.brand = Some(b.to_string());
            }
        }
    }
}

// --------------------------------------------------------------------- France (opendata, sans clé)

fn parse_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn france_cache_path() -> PathBuf {
    paths::cache().join("pitstop-france.json")
}

fn fetch_all_french_stations() -> Result<Vec<Station>> {
    let data = get_bytes(FRANCE_API_URL, 30).context("API prix carburants France")?;
    let records: Vec<Value> = serde_json::from_slice(&data).context("réponse France invalide")?;
    let now = now_s();
    let mut out = vec![];
    for r in &records {
        let Some(id) = r.get("id").and_then(Value::as_str).or_else(|| r.get("id").map(|v| v.to_string()).as_deref().map(|_| "")).map(String::from)
            .filter(|s| !s.is_empty()).or_else(|| r.get("id").map(|v| v.to_string())) else { continue };
        let mut lat = r.get("latitude").and_then(parse_f64);
        let mut lon = r.get("longitude").and_then(parse_f64);
        if lat.is_none() || lon.is_none() {
            lat = r["geom"].get("lat").and_then(parse_f64);
            lon = r["geom"].get("lon").and_then(parse_f64);
        }
        let (Some(mut lat), Some(mut lon)) = (lat, lon) else { continue };
        if lat.abs() > 1000.0 {
            lat /= 100000.0;
        }
        if lon.abs() > 1000.0 {
            lon /= 100000.0;
        }
        let mut prices = HashMap::new();
        for (field, key) in [("sp95_prix", "sp95"), ("sp98_prix", "sp98"), ("e10_prix", "e10"), ("gazole_prix", "gazole")] {
            if let Some(p) = r.get(field).and_then(parse_f64).filter(|p| *p > 0.0) {
                prices.insert(key.to_string(), p);
            }
        }
        out.push(Station {
            id,
            country: "FR".into(),
            brand: r.get("brand").and_then(Value::as_str).map(String::from),
            name: r.get("brand").or_else(|| r.get("name")).and_then(Value::as_str).map(String::from),
            address: r.get("adresse").and_then(Value::as_str).unwrap_or_default().into(),
            city: r.get("ville").and_then(Value::as_str).unwrap_or_default().into(),
            post_code: r.get("cp").and_then(Value::as_str).unwrap_or_default().into(),
            latitude: lat,
            longitude: lon,
            prices,
            updated_at_s: now, // l'export France n'a pas de date par ligne exploitable simplement ; on retient l'heure de collecte
        });
    }
    Ok(out)
}

fn french_stations_around(lat: f64, lon: f64, radius_km: f64) -> Vec<Station> {
    let path = france_cache_path();
    let fresh = std::fs::metadata(&path).and_then(|m| m.modified()).ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d.as_secs() < FRANCE_CACHE_TTL_S);
    let all: Vec<Station> = if fresh {
        std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    } else {
        match fetch_all_french_stations() {
            Ok(stations) => {
                let _ = std::fs::create_dir_all(paths::cache());
                let _ = std::fs::write(&path, serde_json::to_vec(&stations).unwrap_or_default());
                stations
            }
            Err(e) => {
                eprintln!("Pitstop France : {e:#}, utilisation du cache précédent si disponible");
                std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
            }
        }
    };
    all.into_iter().filter(|s| haversine_km(lat, lon, s.latitude, s.longitude) <= radius_km).collect()
}

// --------------------------------------------------------------------- Allemagne (Tankerkoenig, clé optionnelle)

fn german_stations_around(lat: f64, lon: f64, radius_km: f64) -> Vec<Station> {
    let Ok(api_key) = std::env::var("TANKERKOENIG_API_KEY") else { return vec![] }; // pas de clé : ignoré, pas d'erreur
    let cache_path = cache_dir().join(format!("de_{}.json", overpass_cell_key(lat, lon, radius_km)));
    let fresh = std::fs::metadata(&cache_path).and_then(|m| m.modified()).ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|d| d.as_secs() < TANKERKOENIG_TTL_S);
    if fresh {
        if let Ok(s) = std::fs::read_to_string(&cache_path) {
            if let Ok(stations) = serde_json::from_str(&s) {
                return stations;
            }
        }
    }
    let clamped = radius_km.min(25.0);
    let url = format!("{TANKERKOENIG_URL}?lat={lat}&lng={lon}&rad={clamped}&type=all&sort=dist&apikey={api_key}");
    let now = now_s();
    let stations: Vec<Station> = (|| -> Result<Vec<Station>> {
        let data = get_bytes(&url, 15)?;
        let payload: Value = serde_json::from_slice(&data)?;
        if !payload["ok"].as_bool().unwrap_or(false) {
            anyhow::bail!("Tankerkoenig : {}", payload["message"].as_str().unwrap_or("erreur inconnue"));
        }
        let mut out = vec![];
        for st in payload["stations"].as_array().cloned().unwrap_or_default() {
            let Some(id) = st.get("id").and_then(Value::as_str) else { continue };
            let (Some(lat), Some(lon)) = (st.get("lat").and_then(parse_f64), st.get("lng").and_then(parse_f64)) else { continue };
            let mut prices = HashMap::new();
            if let Some(p) = st.get("e5").and_then(parse_f64).filter(|p| *p > 0.0) { prices.insert("sp98".into(), p); }
            if let Some(p) = st.get("e10").and_then(parse_f64).filter(|p| *p > 0.0) { prices.insert("e10".into(), p); }
            if let Some(p) = st.get("diesel").and_then(parse_f64).filter(|p| *p > 0.0) { prices.insert("gazole".into(), p); }
            let place = st.get("place").and_then(Value::as_str).unwrap_or_default();
            let (post_code, city) = place.split_once(' ').filter(|(p, _)| p.len() == 5 && p.chars().all(|c| c.is_ascii_digit()))
                .map(|(p, c)| (p.to_string(), c.to_string())).unwrap_or(("".into(), place.into()));
            out.push(Station {
                id: id.into(),
                country: "DE".into(),
                brand: st.get("brand").and_then(Value::as_str).map(String::from),
                name: st.get("name").and_then(Value::as_str).map(String::from),
                address: st.get("street").and_then(Value::as_str).unwrap_or_default().into(),
                city,
                post_code,
                latitude: lat,
                longitude: lon,
                prices,
                updated_at_s: now,
            });
        }
        Ok(out)
    })().unwrap_or_default();
    let _ = std::fs::create_dir_all(cache_dir());
    let _ = std::fs::write(&cache_path, serde_json::to_vec(&stations).unwrap_or_default());
    stations
}

// --------------------------------------------------------------------- recherche + coût

struct Bbox { min_lat: f64, max_lat: f64, min_lon: f64, max_lon: f64 }
const FR_BBOX: Bbox = Bbox { min_lat: 41.0, max_lat: 51.5, min_lon: -5.5, max_lon: 10.0 };
const DE_BBOX: Bbox = Bbox { min_lat: 47.2, max_lat: 55.1, min_lon: 5.8, max_lon: 15.2 };

fn overlaps(lat: f64, lon: f64, radius_km: f64, b: &Bbox) -> bool {
    let pad = radius_km / 111.12;
    lat + pad >= b.min_lat && lat - pad <= b.max_lat && lon + pad >= b.min_lon && lon - pad <= b.max_lon
}

/// Coût total EUR d'un ravitaillement à cette station : prix au litre (avec pénalité de
/// fraîcheur au-delà de 12h), plus l'aller-retour du détour si `exclude_distance` est faux.
fn effective_cost(station: &Station, distance_km: f64, fuel: Fuel, settings: &Settings) -> Result_ {
    let price = station.prices.get(fuel.key()).copied();
    let age_h = (now_s().saturating_sub(station.updated_at_s)) as f64 / 3600.0;
    let freshness_penalty = if age_h > 12.0 { (0.002 * (age_h - 12.0)).min(0.15) } else { 0.0 };
    let effective_price = price.map(|p| p + freshness_penalty);
    let detour_fuel_l = if settings.exclude_distance { 0.0 } else { distance_km * 2.0 * settings.consumption_l100 / 100.0 };
    let detour_cost = effective_price.map(|p| detour_fuel_l * p).unwrap_or(0.0);
    let fuel_cost = effective_price.map(|p| settings.fill_size_l * p).unwrap_or(0.0);
    Result_ {
        id: station.id.clone(),
        country: station.country.clone(),
        brand: station.brand.clone(),
        name: station.name.clone(),
        address: station.address.clone(),
        city: station.city.clone(),
        post_code: station.post_code.clone(),
        latitude: station.latitude,
        longitude: station.longitude,
        has_price: price.is_some(),
        price_l: price,
        distance_km: (distance_km * 100.0).round() / 100.0,
        detour_cost_eur: (detour_cost * 1000.0).round() / 1000.0,
        total_fuel_cost_eur: (fuel_cost * 1000.0).round() / 1000.0,
        total_cost_eur: ((fuel_cost + detour_cost) * 100.0).round() / 100.0,
        freshness_penalty_eur: (freshness_penalty * 10000.0).round() / 10000.0,
    }
}

/// Stations autour d'un point, triées par coût total (avec prix d'abord), puis distance.
pub fn stations_around(lat: f64, lon: f64, radius_km: f64, fuel: Fuel, settings: &Settings) -> Vec<Result_> {
    let radius_km = radius_km.clamp(1.0, 100.0);
    let mut stations = vec![];
    if overlaps(lat, lon, radius_km, &FR_BBOX) {
        stations.extend(french_stations_around(lat, lon, radius_km));
    }
    if overlaps(lat, lon, radius_km, &DE_BBOX) {
        stations.extend(german_stations_around(lat, lon, radius_km));
    }
    enrich_brands(&mut stations, lat, lon, radius_km);
    let mut results: Vec<Result_> = stations.iter()
        .map(|s| effective_cost(s, haversine_km(lat, lon, s.latitude, s.longitude), fuel, settings))
        .filter(|r| r.distance_km <= radius_km)
        .collect();
    results.sort_by(|a, b| {
        match (a.has_price, b.has_price) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            (false, false) => return a.distance_km.total_cmp(&b.distance_km),
            _ => {}
        }
        a.total_cost_eur.total_cmp(&b.total_cost_eur).then(a.distance_km.total_cmp(&b.distance_km))
    });
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_zero_for_same_point() {
        assert!(haversine_km(45.0, 4.0, 45.0, 4.0) < 1e-9);
    }

    #[test]
    fn haversine_known_distance_paris_lyon() {
        // ~392 km à vol d'oiseau
        let d = haversine_km(48.8566, 2.3522, 45.7640, 4.8357);
        assert!((d - 392.0).abs() < 5.0, "distance obtenue {d}");
    }

    #[test]
    fn effective_cost_without_price_is_zero_but_flagged() {
        let station = Station {
            id: "x".into(), country: "FR".into(), brand: None, name: None, address: "".into(),
            city: "".into(), post_code: "".into(), latitude: 0.0, longitude: 0.0,
            prices: HashMap::new(), updated_at_s: now_s(),
        };
        let settings = Settings { fill_size_l: 10.0, consumption_l100: 5.0, exclude_distance: false };
        let r = effective_cost(&station, 3.0, Fuel::Sp95, &settings);
        assert!(!r.has_price);
        assert_eq!(r.total_cost_eur, 0.0);
    }
}
