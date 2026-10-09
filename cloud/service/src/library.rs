//! Bibliothèque : tous les rushs du client, regroupés en balades à partir de l'index, avec leurs
//! marqueurs (garder, favori, corbeille), la place occupée et les suggestions de nettoyage.
//!
//! Routes (JSON) :
//!   GET  /api/bibliotheque                              → {bytes, quota_bytes, rides, suggestions}
//!        (une session analysée porte aussi sa vignette, sa distance, son GPS et ses moments forts)
//!   POST /api/bibliotheque/marque  {session, mark}       → {ok}   (mark : garder, favori, corbeille ou null)
//!   POST /api/bibliotheque/alleger {session, confirm}    → {ok, freed_bytes}   supprime les originaux, garde les aperçus
//!   POST /api/bibliotheque/purge                         → {ok, sessions, freed_bytes}   vide la corbeille échue

use std::collections::{BTreeMap, HashMap};

use aws_sdk_dynamodb::types::AttributeValue;
use axum::http::StatusCode;
use axum::Json;
use bike360_core::rides;
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::account::Scope;
use crate::{aws, Fail};

/// Marqueurs qu'un client peut poser sur une session.
const MARKS: [&str; 3] = ["garder", "favori", "corbeille"];
/// En dessous, une session est proposée au nettoyage (déclenchement par erreur, essai).
const SHORT_S: f64 = 60.0;
/// Part du temps en mouvement sous laquelle la caméra a sans doute tourné à l'arrêt.
const IDLE_SHARE: f64 = 0.1;
/// Couverture GPS minimale pour se fier au temps en mouvement.
const GPS_MIN: f64 = 0.3;
/// Durée de validité de l'adresse d'une vignette.
const THUMB_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Résumé d'analyse d'une session, écrit dans l'index par l'exécutant des tâches.
struct Analysis {
    duration_s: f64,
    gps: f64,
    candidates: f64,
    distance_km: Option<f64>,
    moving_s: Option<f64>,
    thumb: Option<String>,
}

/// Fichier d'un rush tel que l'index le décrit.
struct Rush {
    name: String,
    key: String,
    originals: bool,
    bytes: u64,
    class: String,
    camera: Option<String>,
    serial: Option<String>,
    duration_s: f64,
}

/// Session : ses aperçus, ses originaux et son marqueur.
#[derive(Default)]
struct Session {
    previews: Vec<Rush>,
    originals: Vec<Rush>,
    mark: Option<String>,
    marked_at: Option<String>,
    analysis: Option<Analysis>,
}

impl Session {
    fn files(&self) -> impl Iterator<Item = &Rush> {
        self.previews.iter().chain(&self.originals)
    }
    /// Durée de l'analyse si elle est faite ; sinon d'après les aperçus (à défaut, les originaux),
    /// un fichier par segment.
    fn duration_s(&self) -> f64 {
        if let Some(a) = &self.analysis {
            return a.duration_s;
        }
        let of = if self.previews.is_empty() { &self.originals } else { &self.previews };
        of.iter().map(|r| r.duration_s).sum()
    }
    fn bytes(&self) -> u64 {
        self.files().map(|r| r.bytes).sum()
    }
}

fn text(item: &HashMap<String, AttributeValue>, key: &str) -> Option<String> {
    item.get(key).and_then(|v| v.as_s().ok()).cloned()
}

fn number(item: &HashMap<String, AttributeValue>, key: &str) -> f64 {
    item.get(key).and_then(|v| v.as_n().ok()).and_then(|n| n.parse().ok()).unwrap_or(0.0)
}

impl Scope {
    fn table(&self) -> Result<&str, Fail> {
        self.table.as_deref().ok_or_else(|| Fail(StatusCode::SERVICE_UNAVAILABLE, "index non configuré (--table)".into()))
    }

    fn pk(&self) -> AttributeValue {
        AttributeValue::S(format!("client#{}", self.client))
    }

    /// Toutes les lignes de l'index du client dont la clé de tri commence par `prefix`.
    async fn rows(&self, prefix: &str) -> Result<Vec<HashMap<String, AttributeValue>>, Fail> {
        let table = self.table()?;
        let (mut out, mut from) = (vec![], None);
        loop {
            let page = self.db.query().table_name(table)
                .key_condition_expression("pk = :c and begins_with(sk, :p)")
                .expression_attribute_values(":c", self.pk())
                .expression_attribute_values(":p", AttributeValue::S(prefix.into()))
                .set_exclusive_start_key(from.take())
                .send().await.map_err(aws("lecture de l'index"))?;
            out.extend(page.items().iter().cloned());
            match page.last_evaluated_key() {
                Some(key) => from = Some(key.clone()),
                None => return Ok(out),
            }
        }
    }

    /// Sessions du client (identifiant → contenu), marqueurs compris.
    async fn sessions(&self) -> Result<BTreeMap<String, Session>, Fail> {
        let mut out: BTreeMap<String, Session> = BTreeMap::new();
        for item in self.rows("rush#").await? {
            let (Some(sk), Some(key), Some(session)) = (text(&item, "sk"), text(&item, "cle"), text(&item, "session")) else { continue };
            let rush = Rush {
                name: sk.trim_start_matches("rush#").to_string(),
                originals: key.starts_with("originaux/"),
                key,
                bytes: number(&item, "octets") as u64,
                class: text(&item, "classe").unwrap_or_default(),
                camera: text(&item, "camera_modele"),
                serial: text(&item, "camera_serie"),
                duration_s: number(&item, "duree_s"),
            };
            let s = out.entry(session).or_default();
            if rush.originals { s.originals.push(rush) } else { s.previews.push(rush) }
        }
        // résumés d'analyse ; les parties d'un enregistrement en boucle rejoignent leur bloc
        for item in self.rows("session#").await? {
            let (Some(id), Some(block)) = (text(&item, "sk").map(|sk| sk.trim_start_matches("session#").to_string()), text(&item, "bloc")) else { continue };
            if id != block {
                if let Some(part) = out.remove(&id) {
                    let b = out.entry(block).or_default();
                    b.previews.extend(part.previews);
                    b.originals.extend(part.originals);
                }
                continue;
            }
            let opt = |k: &str| item.contains_key(k).then(|| number(&item, k));
            out.entry(id).or_default().analysis = Some(Analysis {
                duration_s: number(&item, "duree_s"), gps: number(&item, "gps"), candidates: number(&item, "candidats"),
                distance_km: opt("distance_km"), moving_s: opt("mobile_s"), thumb: text(&item, "vignette"),
            });
        }
        out.retain(|_, s| !s.previews.is_empty() || !s.originals.is_empty());
        for item in self.rows("marque#").await? {
            let Some(id) = text(&item, "sk").map(|sk| sk.trim_start_matches("marque#").to_string()) else { continue };
            if let Some(s) = out.get_mut(&id) {
                s.mark = text(&item, "marque");
                s.marked_at = text(&item, "depuis");
            }
        }
        Ok(out)
    }

    /// Supprime des rushs du stockage puis de l'index ; renvoie les octets libérés.
    async fn delete(&self, rushes: &[&Rush]) -> Result<u64, Fail> {
        let table = self.table()?;
        let mut freed = 0;
        for r in rushes {
            self.s3.delete_object().bucket(&self.bucket).key(&r.key).send().await.map_err(aws("suppression"))?;
            self.db.delete_item().table_name(table).key("pk", self.pk())
                .key("sk", AttributeValue::S(format!("rush#{}", r.name))).send().await.map_err(aws("mise à jour de l'index"))?;
            freed += r.bytes;
        }
        Ok(freed)
    }
}

/// Début d'une session (heure de la caméra) en secondes, pour le regroupement en balades.
fn start_of(id: &str) -> f64 {
    let (date, time) = (id.get(4..12).unwrap_or_default(), id.get(13..19).unwrap_or_default());
    NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M%S").map_or(0.0, |d| d.and_utc().timestamp() as f64)
}

fn days_since(iso: &str) -> f64 {
    DateTime::parse_from_rfc3339(iso).map_or(0.0, |t| (Utc::now() - t.with_timezone(&Utc)).num_seconds() as f64 / 86400.0)
}

pub async fn list(c: Scope) -> Result<Json<Value>, Fail> {
    let sessions = c.sessions().await?;
    let spans: Vec<rides::Span> = sessions.iter().map(|(id, s)| {
        let start = start_of(id);
        rides::Span { id, start, end: start + s.duration_s(), camera: s.files().find_map(|r| r.serial.as_deref()) }
    }).collect();
    let groups = rides::group(&spans);
    let mut by_ride: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut suggestions = vec![];
    for ((id, s), (ride, angles)) in sessions.iter().zip(&groups) {
        let side = |of: &[Rush]| json!({"files": of.len(), "bytes": of.iter().map(|r| r.bytes).sum::<u64>(),
                                        "class": of.first().map(|r| r.class.clone())});
        let trash_days = s.marked_at.as_deref().filter(|_| s.mark.as_deref() == Some("corbeille")).map(days_since);
        let a = s.analysis.as_ref();
        let reason = if s.mark.is_some() {
            None
        } else if s.duration_s() < SHORT_S {
            Some(("courte", format!("Session de {:.0} s : sans doute un déclenchement par erreur.", s.duration_s())))
        } else if a.is_some_and(|a| a.gps >= GPS_MIN && a.moving_s.is_some_and(|m| m < IDLE_SHARE * a.duration_s)) {
            Some(("arret", "La moto ne roule presque pas : la caméra a sans doute tourné à l'arrêt.".to_string()))
        } else if a.is_some_and(|a| a.candidates == 0.0) {
            Some(("sans-moment-fort", "Aucun moment fort repéré dans cette session.".to_string()))
        } else {
            None
        };
        if let Some((reason, text)) = reason {
            suggestions.push(json!({"session": id, "reason": reason, "bytes": s.bytes(), "text": text}));
        }
        let thumb = match a.and_then(|a| a.thumb.as_deref()) {
            Some(key) => c.s3.get_object().bucket(&c.bucket).key(key)
                .presigned(aws_sdk_s3::presigning::PresigningConfig::expires_in(THUMB_TTL).map_err(aws("signature"))?)
                .await.map(|r| r.uri().to_string()).ok(),
            None => None,
        };
        by_ride.entry(ride).or_default().push(json!({
            "id": id, "date": id.get(4..12), "time": id.get(13..19), "duration_s": s.duration_s(), "bytes": s.bytes(),
            "camera": s.files().find_map(|r| r.camera.clone()), "angles": angles,
            "previews": side(&s.previews), "originals": side(&s.originals),
            "analysed": a.is_some(), "thumb": thumb, "distance_km": a.and_then(|a| a.distance_km),
            "gps_coverage": a.map(|a| a.gps), "candidates": a.map(|a| a.candidates),
            "mark": s.mark, "trash_days_left": trash_days.map(|d| (c.trash_days - d).max(0.0).ceil()),
        }));
    }
    let rides: Vec<Value> = by_ride.into_iter().rev().map(|(ride, list)| {
        let bytes: u64 = list.iter().filter_map(|s| s["bytes"].as_u64()).sum();
        json!({"id": ride, "date": ride.get(4..12), "bytes": bytes, "sessions": list})
    }).collect();
    let bytes: u64 = sessions.values().map(Session::bytes).sum();
    Ok(Json(json!({"bytes": bytes, "quota_bytes": c.quota_bytes, "trash_days": c.trash_days, "rides": rides, "suggestions": suggestions})))
}

#[derive(Deserialize)]
pub struct Mark {
    session: String,
    mark: Option<String>,
}

/// Pose ou retire le marqueur d'une session. La corbeille ne supprime rien : elle date la demande.
pub async fn mark(c: Scope, Json(b): Json<Mark>) -> Result<Json<Value>, Fail> {
    let table = c.table()?;
    if !c.sessions().await?.contains_key(&b.session) {
        return Err(Fail(StatusCode::NOT_FOUND, "session inconnue".into()));
    }
    let sk = AttributeValue::S(format!("marque#{}", b.session));
    match b.mark.as_deref() {
        None => {
            c.db.delete_item().table_name(table).key("pk", c.pk()).key("sk", sk).send().await.map_err(aws("marqueur"))?;
        }
        Some(m) if MARKS.contains(&m) => {
            c.db.put_item().table_name(table).item("pk", c.pk()).item("sk", sk)
                .item("marque", AttributeValue::S(m.into()))
                .item("depuis", AttributeValue::S(Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()))
                .send().await.map_err(aws("marqueur"))?;
        }
        Some(_) => return Err(Fail(StatusCode::BAD_REQUEST, "marqueur inconnu".into())),
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct Lighten {
    session: String,
    #[serde(default)]
    confirm: bool,
}

/// Allège une session : ses originaux sont supprimés pour de bon, ses aperçus restent.
pub async fn lighten(c: Scope, Json(b): Json<Lighten>) -> Result<Json<Value>, Fail> {
    if !b.confirm {
        return Err(Fail(StatusCode::BAD_REQUEST, "suppression définitive : confirmation requise".into()));
    }
    let sessions = c.sessions().await?;
    let s = sessions.get(&b.session).ok_or_else(|| Fail(StatusCode::NOT_FOUND, "session inconnue".into()))?;
    if s.previews.is_empty() {
        return Err(Fail(StatusCode::CONFLICT, "pas d'aperçu : alléger ferait disparaître la session".into()));
    }
    let freed = c.delete(&s.originals.iter().collect::<Vec<_>>()).await?;
    Ok(Json(json!({"ok": true, "freed_bytes": freed})))
}

/// Vide la corbeille : supprime pour de bon les sessions qui y sont depuis le délai de garde.
pub async fn purge(c: Scope) -> Result<Json<Value>, Fail> {
    let table = c.table()?;
    let (mut freed, mut count) = (0, 0);
    for (id, s) in c.sessions().await? {
        let due = s.mark.as_deref() == Some("corbeille") && s.marked_at.as_deref().is_some_and(|t| days_since(t) >= c.trash_days);
        if !due {
            continue;
        }
        freed += c.delete(&s.files().collect::<Vec<_>>()).await?;
        // ce que l'analyse a produit pour cette session part avec elle
        for key in [format!("donnees/{}/vignettes/{id}.jpg", c.client), format!("donnees/{}/cache/{id}.json", c.client)] {
            c.s3.delete_object().bucket(&c.bucket).key(key).send().await.map_err(aws("suppression"))?;
        }
        // ses clips, leur historique et ses zones de floutage, enregistrés par l'atelier
        for dir in ["selections", "selections/historique", "privacy"] {
            let prefix = format!("donnees/{}/atelier/{dir}/{id}", c.client);
            let found = c.s3.list_objects_v2().bucket(&c.bucket).prefix(&prefix).send().await.map_err(aws("suppression"))?;
            for key in found.contents().iter().filter_map(|o| o.key()) {
                c.s3.delete_object().bucket(&c.bucket).key(key).send().await.map_err(aws("suppression"))?;
            }
        }
        for sk in [format!("marque#{id}"), format!("session#{id}")] {
            c.db.delete_item().table_name(table).key("pk", c.pk()).key("sk", AttributeValue::S(sk))
                .send().await.map_err(aws("mise à jour de l'index"))?;
        }
        count += 1;
    }
    Ok(Json(json!({"ok": true, "sessions": count, "freed_bytes": freed})))
}
