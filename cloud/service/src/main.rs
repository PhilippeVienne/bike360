//! Service commun à tous les clients : l'Envoi (ci-dessous) et la Bibliothèque (library.rs).
//!
//! Module Envoi : le navigateur dépose les rushs directement dans S3, par morceaux, et reprend
//! après une coupure. Ce service ne voit jamais passer les vidéos : il ouvre l'envoi, signe les
//! adresses des morceaux et assemble le fichier à la fin.
//!
//! À l'arrivée d'un rush, sa télémétrie est lue par lectures partielles (caméra, durée), le rush est
//! inscrit dans l'index, son analyse est déposée dans la file et l'atelier du client, s'il tourne, le reçoit.
//!
//! Usage : bike360-envoi --bucket NOM [--table NOM] [--queue URL] [--issuer URL --app-client ID]
//!                      [--atelier-bin CHEMIN] [--ui DOSSIER] [--port 8370]
//! Routes (JSON) :
//!   POST /api/envoi/start    {name, size}            → {done} ou {upload_id, part_size, parts, received}
//!   POST /api/envoi/urls     {name, upload_id, parts} → {urls: {numéro: adresse signée}}
//!   POST /api/envoi/complete {name, size, upload_id}  → {ok, key, camera, duration_s, indexed}
//!   GET  /api/envoi/rushs                             → [{name, kind, size, class}]

mod account;
mod atelier;
mod library;
mod payment;
mod plans;
mod upkeep;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, StorageClass};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bike360_core::insta360::{self, Camera, TRAILER_TAIL};

use crate::account::{Auth, Scope};
use clap::Parser;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

/// Durée de validité d'une adresse signée de morceau.
const URL_TTL: Duration = Duration::from_secs(3600);
/// Adresses signées délivrées par appel.
const MAX_URLS: usize = 100;
/// S3 refuse plus de 10 000 morceaux par fichier.
const MAX_PARTS: u64 = 10_000;

#[derive(Parser)]
#[command(about = "Envoi des rushs Bike360 vers S3, par morceaux et avec reprise")]
struct Args {
    /// Compartiment S3 des rushs
    #[arg(long, env = "BIKE360_BUCKET")]
    bucket: String,
    /// Client servi quand les comptes ne sont pas activés (essai local sur une seule machine)
    #[arg(long, env = "BIKE360_CLIENT", default_value = "demo")]
    client: String,
    /// Émetteur des jetons (groupe d'utilisateurs Cognito) : active les comptes, avec --app-client
    #[arg(long, env = "BIKE360_ISSUER", requires = "app_client")]
    issuer: Option<String>,
    /// Application cliente Cognito
    #[arg(long, env = "BIKE360_APP_CLIENT")]
    app_client: Option<String>,
    /// Taille d'un morceau en Mo (5 au minimum, imposé par S3)
    #[arg(long, env = "BIKE360_PART_MB", default_value_t = 16)]
    part_mb: u64,
    /// Table DynamoDB de l'index (sans elle, les rushs ne sont pas inscrits)
    #[arg(long, env = "BIKE360_TABLE")]
    table: Option<String>,
    /// File des tâches : une analyse y est déposée à l'arrivée de chaque aperçu
    #[arg(long, env = "BIKE360_QUEUE")]
    queue: Option<String>,
    /// Exécutable de l'atelier (bike360-server) : permet d'ouvrir un atelier par compte, à la demande
    #[arg(long, env = "BIKE360_ATELIER_BIN")]
    atelier_bin: Option<std::path::PathBuf>,
    /// Dossier de travail des ateliers
    #[arg(long, default_value = "/tmp/bike360-ateliers")]
    atelier_work: std::path::PathBuf,
    /// Nom ou adresse de cette machine vue du navigateur du client
    #[arg(long, default_value = "127.0.0.1")]
    atelier_host: String,
    /// Minutes d'inactivité avant l'arrêt d'un atelier
    #[arg(long, default_value_t = 30)]
    atelier_idle_min: u64,
    /// Grille des paliers (JSON) à la place de celle par défaut
    #[arg(long, env = "BIKE360_PLANS")]
    plans: Option<std::path::PathBuf>,
    /// Adresse publique du site, où le prestataire de paiement renvoie le client
    #[arg(long, env = "BIKE360_SITE")]
    site: Option<String>,
    /// Jours de garde en corbeille avant suppression définitive
    #[arg(long, env = "BIKE360_TRASH_DAYS", default_value_t = 30.0)]
    trash_days: f64,
    /// Jours d'essai gratuit à compter du premier envoi ; ensuite les rushs du compte sont supprimés
    #[arg(long, env = "BIKE360_TRIAL_DAYS", default_value_t = 7.0)]
    trial_days: f64,
    /// Jours d'accès aux rushs après la fin d'un abonnement
    #[arg(long, env = "BIKE360_ACCESS_DAYS", default_value_t = 30.0)]
    access_days: f64,
    /// Jours de garde en archive profonde après cet accès, avant suppression
    #[arg(long, env = "BIKE360_ARCHIVE_DAYS", default_value_t = 180.0)]
    archive_days: f64,
    /// Achat minimal de crédit d'export, en minutes
    #[arg(long, default_value_t = 60)]
    credit_min: u32,
    /// Prix d'une minute de crédit d'export, en euros (3 € de l'heure)
    #[arg(long, default_value_t = 0.05)]
    credit_eur: f64,
    /// Prix de la récupération de rushs archivés, en euros par tranche de 100 Go
    #[arg(long, default_value_t = 2.0)]
    recovery_eur_100go: f64,
    /// Minutes entre deux passages sur les échéances des comptes
    #[arg(long, env = "BIKE360_SWEEP_MIN", default_value_t = 60.0)]
    sweep_min: f64,
    /// Dossier de l'interface web à servir (pages envoi.html et bibliotheque.html)
    #[arg(long)]
    ui: Option<String>,
    #[arg(long, default_value_t = 8370)]
    port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

pub struct Ctx {
    s3: aws_sdk_s3::Client,
    db: aws_sdk_dynamodb::Client,
    sqs: aws_sdk_sqs::Client,
    queue: Option<String>,
    /// Comptes activés : chaque requête agit pour le compte connecté. Sinon, pour `client`.
    auth: Option<Auth>,
    bucket: String,
    client: String,
    part_size: u64,
    table: Option<String>,
    launcher: Option<atelier::Launcher>,
    plans: Vec<plans::Plan>,
    payment: Option<payment::Payment>,
    trash_days: f64,
    policy: plans::Policy,
}

/// Ce que la fin d'un rush dit de lui, sans le télécharger.
#[derive(Default)]
struct Telemetry {
    camera: Option<Camera>,
    /// Durée déduite de l'IMU (1 000 mesures de 20 octets par seconde).
    duration_s: Option<f64>,
}

/// Erreur renvoyée au navigateur : statut et message.
pub struct Fail(pub StatusCode, pub String);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

/// Une erreur d'AWS n'est pas détaillée au navigateur ; elle est écrite dans le journal.
fn aws<E: std::fmt::Debug>(what: &'static str) -> impl FnOnce(E) -> Fail {
    move |e| {
        eprintln!("{what} : {e:?}");
        Fail(StatusCode::BAD_GATEWAY, format!("{what} : stockage indisponible"))
    }
}

/// Nature d'un rush d'après son nom : (préfixe du compartiment, classe de stockage).
/// Les aperçus sont lus pendant le montage, les originaux seulement à l'export.
fn kind(name: &str) -> Option<(&'static str, StorageClass)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^(VID|LRV)_\d{8}_\d{6}_\d{2}_\d{3}\.(insv|lrv)$").unwrap());
    let m = re.captures(name)?;
    match (m[1].to_ascii_uppercase().as_str(), m[2].to_ascii_lowercase().as_str()) {
        ("LRV", "lrv") => Some(("apercus", StorageClass::IntelligentTiering)),
        ("VID", "insv") => Some(("originaux", StorageClass::GlacierIr)),
        _ => None,
    }
}

impl Scope {
    /// Clé S3 d'un rush du client ; le nom est validé, donc jamais de chemin fourni par le navigateur.
    fn key(&self, name: &str) -> Result<(String, StorageClass), Fail> {
        let (prefix, class) = kind(name).ok_or_else(|| Fail(StatusCode::BAD_REQUEST, "nom de fichier Insta360 attendu (VID_….insv ou LRV_….lrv)".into()))?;
        Ok((format!("{prefix}/{}/{name}", self.client), class))
    }

    /// Nombre de morceaux et taille attendue du morceau `n` (à partir de 1) pour un fichier de `size` octets.
    fn layout(&self, size: u64) -> Result<u64, Fail> {
        let count = size.div_ceil(self.part_size).max(1);
        if size == 0 || count > MAX_PARTS {
            return Err(Fail(StatusCode::BAD_REQUEST, "fichier vide ou trop gros pour la taille de morceau configurée".into()));
        }
        Ok(count)
    }

    fn part_len(&self, size: u64, n: u64) -> u64 {
        (size - (n - 1) * self.part_size).min(self.part_size)
    }

    /// Morceaux déjà reçus par S3 pour cet envoi : numéro → (taille, étiquette).
    async fn received(&self, key: &str, upload_id: &str) -> Result<BTreeMap<u64, (u64, String)>, Fail> {
        let (mut out, mut marker) = (BTreeMap::new(), None::<String>);
        loop {
            let page = self.s3.list_parts().bucket(&self.bucket).key(key).upload_id(upload_id)
                .set_part_number_marker(marker.take()).send().await.map_err(aws("liste des morceaux"))?;
            for p in page.parts() {
                if let (Some(n), Some(size), Some(tag)) = (p.part_number(), p.size(), p.e_tag()) {
                    out.insert(n as u64, (size as u64, tag.to_string()));
                }
            }
            match (page.is_truncated(), page.next_part_number_marker()) {
                (Some(true), Some(next)) => marker = Some(next.to_string()),
                _ => return Ok(out),
            }
        }
    }
}

impl Scope {
    async fn range(&self, key: &str, offset: u64, length: usize) -> Result<Vec<u8>> {
        let out = self.s3.get_object().bucket(&self.bucket).key(key)
            .range(format!("bytes={offset}-{}", offset + length as u64 - 1)).send().await?;
        Ok(out.body.collect().await?.into_bytes().to_vec())
    }

    /// Lit la télémétrie d'un rush en deux lectures partielles : l'index de fin, puis les métadonnées.
    async fn telemetry(&self, key: &str, size: u64) -> Result<Telemetry> {
        if size < TRAILER_TAIL as u64 {
            return Ok(Telemetry::default());
        }
        let tail = self.range(key, size - TRAILER_TAIL as u64, TRAILER_TAIL).await?;
        let index = insta360::trailer_index(&tail, size);
        let mut t = Telemetry { duration_s: index.iter().find(|r| r.0 == 0x03).map(|r| r.2 as f64 / 20.0 / 1000.0), ..Default::default() };
        if let Some((_, offset, length)) = index.iter().find(|r| r.0 == 0x0101) {
            t.camera = insta360::camera_of_meta(&self.range(key, *offset, *length).await?);
        }
        Ok(t)
    }

    /// Inscrit le rush dans l'index du client (pk = client, sk = rush).
    async fn index(&self, table: &str, name: &str, key: &str, size: u64, class: &StorageClass, t: &Telemetry) -> Result<()> {
        let s = |v: &str| AttributeValue::S(v.to_string());
        let parts: Vec<&str> = name.split(['_', '.']).collect();   // VID _ date _ heure _ 00 _ idx . insv
        let mut put = self.db.put_item().table_name(table)
            .item("pk", s(&format!("client#{}", self.client)))
            .item("sk", s(&format!("rush#{name}")))
            .item("cle", s(key))
            .item("nature", s(key.split('/').next().unwrap_or_default()))
            .item("classe", s(class.as_str()))
            .item("octets", AttributeValue::N(size.to_string()))
            .item("session", s(&insta360::session_id(parts[1], parts[2], t.camera.as_ref())))
            .item("recu", s(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        if let Some(c) = &t.camera {
            put = put.item("camera_serie", s(&c.serial)).item("camera_modele", s(&c.model));
        }
        if let Some(d) = t.duration_s {
            put = put.item("duree_s", AttributeValue::N(format!("{d:.1}")));
        }
        put.send().await?;
        Ok(())
    }

    /// Dépose l'analyse de la session dans la file (vignette, moments forts, statistiques pour la Bibliothèque).
    pub(crate) async fn enqueue(&self, session: &str) {
        let Some(queue) = &self.queue else { return };
        let body = json!({"job": "analyse", "client": self.client, "session": session}).to_string();
        if let Err(e) = self.sqs.send_message().queue_url(queue).message_body(body).send().await {
            eprintln!("analyse de {session} non déposée : {e:?}");
        }
    }

}

#[derive(Deserialize)]
struct Start {
    name: String,
    size: u64,
}

/// Ouvre ou reprend l'envoi d'un rush. Un fichier déjà complet n'est pas renvoyé.
async fn start(c: Scope, Json(b): Json<Start>) -> Result<Json<Value>, Fail> {
    let (key, class) = c.key(&b.name)?;
    if let Ok(head) = c.s3.head_object().bucket(&c.bucket).key(&key).send().await {
        if head.content_length() == Some(b.size as i64) {
            return Ok(Json(json!({"done": true, "key": key})));
        }
    }
    c.check_storage(b.size).await?;
    let count = c.layout(b.size)?;
    // envoi déjà commencé pour ce fichier (onglet fermé, coupure) : on le reprend
    let open = c.s3.list_multipart_uploads().bucket(&c.bucket).prefix(&key).send().await.map_err(aws("envois en cours"))?;
    let resumed = open.uploads().iter()
        .filter(|u| u.key() == Some(key.as_str()))
        .max_by_key(|u| u.initiated().map(|t| t.secs()))
        .and_then(|u| u.upload_id())
        .map(String::from);
    let upload_id = match resumed {
        Some(id) => id,
        None => c.s3.create_multipart_upload().bucket(&c.bucket).key(&key).storage_class(class).send().await
            .map_err(aws("ouverture de l'envoi"))?
            .upload_id().map(String::from).ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "envoi sans identifiant".into()))?,
    };
    // un morceau ne compte que s'il a la taille attendue (un morceau d'une autre découpe est renvoyé)
    let received: Vec<u64> = c.received(&key, &upload_id).await?.into_iter()
        .filter(|(n, (size, _))| *n <= count && *size == c.part_len(b.size, *n))
        .map(|(n, _)| n)
        .collect();
    Ok(Json(json!({"done": false, "key": key, "upload_id": upload_id, "part_size": c.part_size, "parts": count, "received": received})))
}

#[derive(Deserialize)]
struct Urls {
    name: String,
    upload_id: String,
    parts: Vec<u64>,
}

/// Adresses signées où le navigateur dépose les morceaux demandés.
async fn urls(c: Scope, Json(b): Json<Urls>) -> Result<Json<Value>, Fail> {
    let (key, _) = c.key(&b.name)?;
    if b.parts.len() > MAX_URLS || b.parts.iter().any(|n| *n == 0 || *n > MAX_PARTS) {
        return Err(Fail(StatusCode::BAD_REQUEST, format!("{MAX_URLS} morceaux au plus par appel, numérotés de 1 à {MAX_PARTS}")));
    }
    let conf = PresigningConfig::expires_in(URL_TTL).map_err(aws("signature"))?;
    let mut out = serde_json::Map::new();
    for n in b.parts {
        let req = c.s3.upload_part().bucket(&c.bucket).key(&key).upload_id(&b.upload_id).part_number(n as i32)
            .presigned(conf.clone()).await.map_err(aws("signature"))?;
        out.insert(n.to_string(), req.uri().into());
    }
    Ok(Json(json!({"urls": out})))
}

#[derive(Deserialize)]
struct Complete {
    name: String,
    size: u64,
    upload_id: String,
}

/// Assemble le fichier quand tous ses morceaux sont là, à la bonne taille.
async fn complete(c: Scope, Json(b): Json<Complete>) -> Result<Json<Value>, Fail> {
    let (key, class) = c.key(&b.name)?;
    let count = c.layout(b.size)?;
    let got = c.received(&key, &b.upload_id).await?;
    let missing: Vec<u64> = (1..=count).filter(|n| got.get(n).map(|p| p.0) != Some(c.part_len(b.size, *n))).collect();
    if !missing.is_empty() {
        return Err(Fail(StatusCode::CONFLICT, format!("{} morceau(x) manquant(s), à commencer par le {}", missing.len(), missing[0])));
    }
    let parts = (1..=count).map(|n| CompletedPart::builder().part_number(n as i32).e_tag(&got[&n].1).build()).collect();
    c.s3.complete_multipart_upload().bucket(&c.bucket).key(&key).upload_id(&b.upload_id)
        .multipart_upload(CompletedMultipartUpload::builder().set_parts(Some(parts)).build())
        .send().await.map_err(aws("assemblage"))?;
    // le fichier est en place : la suite renseigne l'index et l'atelier, et ne remet pas l'envoi en cause
    let t = c.telemetry(&key, b.size).await.unwrap_or_else(|e| {
        eprintln!("télémétrie de {key} illisible : {e:#}");
        Telemetry::default()
    });
    let indexed = match &c.table {
        Some(table) => c.index(table, &b.name, &key, b.size, &class, &t).await.inspect_err(|e| eprintln!("index de {key} : {e:#}")).is_ok(),
        None => false,
    };
    if key.starts_with("apercus/") {
        let parts: Vec<&str> = b.name.split(['_', '.']).collect();
        c.enqueue(&insta360::session_id(parts[1], parts[2], t.camera.as_ref())).await;
        c.atelier_arrival().await;
    }
    Ok(Json(json!({"ok": true, "key": key, "camera": t.camera.map(|c| c.model), "duration_s": t.duration_s, "indexed": indexed})))
}

/// Rushs du client déjà dans le stockage.
async fn rushs(c: Scope) -> Result<Json<Value>, Fail> {
    let mut out = vec![];
    for prefix in ["apercus", "originaux"] {
        let dir = format!("{prefix}/{}/", c.client);
        let mut token = None::<String>;
        loop {
            let page = c.s3.list_objects_v2().bucket(&c.bucket).prefix(&dir).set_continuation_token(token.take())
                .send().await.map_err(aws("liste des rushs"))?;
            for o in page.contents() {
                let name = o.key().and_then(|k| k.strip_prefix(dir.as_str())).unwrap_or_default();
                out.push(json!({"name": name, "kind": prefix, "size": o.size(),
                                "class": o.storage_class().map(|s| s.as_str().to_string())}));
            }
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_string()),
                None => break,
            }
        }
    }
    Ok(Json(Value::Array(out)))
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    anyhow::ensure!(a.part_mb >= 5, "S3 impose des morceaux d'au moins 5 Mo");
    let conf = aws_config::load_from_env().await;
    // un émulateur ou un stockage compatible (AWS_ENDPOINT_URL) s'adresse par chemin, pas par sous-domaine
    let local = std::env::var("AWS_ENDPOINT_URL").is_ok_and(|v| !v.is_empty());
    let s3 = aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&conf).force_path_style(local).build());
    let db = aws_sdk_dynamodb::Client::new(&conf);
    let auth = match (a.issuer, a.app_client) {
        (Some(issuer), Some(app)) => Some(Auth::new(&conf, issuer, app).await.context("activation des comptes")?),
        _ => {
            anyhow::ensure!(a.host == "127.0.0.1" || a.host == "localhost", "sans comptes (--issuer), le service n'écoute que sur cette machine");
            None
        }
    };
    let site = a.site.clone().unwrap_or_else(|| format!("http://{}:{}", a.host, a.port)).trim_end_matches('/').to_string();
    let ctx = Arc::new(Ctx { s3, db, sqs: aws_sdk_sqs::Client::new(&conf), queue: a.queue, auth, bucket: a.bucket, client: a.client, part_size: a.part_mb * 1024 * 1024,
                             table: a.table,
                             launcher: a.atelier_bin.map(|bin| atelier::Launcher::new(bin, a.atelier_work, a.atelier_host, a.atelier_idle_min * 60,
                                                                                   format!("http://127.0.0.1:{}", a.port), site.clone())),
                             plans: match &a.plans {
                                 Some(file) => plans::load(file)?,
                                 None => plans::defaults(),
                             },
                             payment: payment::Payment::from_env(site)?,
                             trash_days: a.trash_days,
                             policy: plans::Policy { trial_days: a.trial_days, access_days: a.access_days, archive_days: a.archive_days,
                                                     credit_min: a.credit_min, credit_eur: a.credit_eur, recovery_eur_100go: a.recovery_eur_100go } });
    let mut app = Router::new()
        .route("/api/envoi/start", post(start))
        .route("/api/envoi/urls", post(urls))
        .route("/api/envoi/complete", post(complete))
        .route("/api/envoi/rushs", get(rushs))
        .route("/api/compte", get(account::status))
        .route("/api/compte/inscription", post(account::sign_up))
        .route("/api/compte/confirmation", post(account::confirm))
        .route("/api/compte/connexion", post(account::sign_in))
        .route("/api/compte/rafraichir", post(account::refresh))
        .route("/api/compte/deconnexion", post(account::sign_out))
        .route("/api/compte/oubli", post(account::forgot))
        .route("/api/compte/reinitialisation", post(account::reset))
        .route("/api/compte/suppression", post(account::delete))
        .route("/api/compte/palier", get(plans::status))
        .route("/api/paiement/commande", post(payment::order))
        .route("/api/paiement/credit", post(payment::credit))
        .route("/api/paiement/resiliation", post(payment::cancel))
        .route("/api/paiement/stripe", post(payment::webhook))
        .route("/api/atelier", get(atelier::status))
        .route("/api/atelier/ouvrir", post(atelier::open))
        .route("/api/atelier/fermer", post(atelier::close))
        .route("/api/interne/originaux", post(atelier::originals))
        .route("/api/bibliotheque", get(library::list))
        .route("/api/bibliotheque/marque", post(library::mark))
        .route("/api/bibliotheque/alleger", post(library::lighten))
        .route("/api/bibliotheque/purge", post(library::purge))
        .with_state(ctx.clone());
    if let Some(ui) = &a.ui {
        app = app.nest_service("/ui", tower_http::services::ServeDir::new(ui));
    }
    tokio::spawn(atelier::watch(ctx.clone()));
    tokio::spawn(upkeep::watch(ctx.clone(), Duration::from_secs_f64((a.sweep_min * 60.0).max(1.0))));
    let listener = tokio::net::TcpListener::bind((a.host.as_str(), a.port)).await.context("ouverture du port")?;
    println!("Service → http://{}:{}/ui/bibliotheque.html (compartiment {}, {})", a.host, a.port, ctx.bucket,
             if ctx.auth.is_some() { "comptes activés".to_string() } else { format!("sans comptes, client {}", ctx.client) });
    // à l'arrêt du service (SIGTERM, Ctrl-C), le travail des ateliers ouverts est enregistré avant de les arrêter
    let stop = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal d'arrêt");
        tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
    };
    axum::serve(listener, app).with_graceful_shutdown(stop).await?;
    atelier::close_all(&ctx).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_decide_prefix_and_class() {
        assert_eq!(kind("LRV_20260920_092259_01_581.lrv").map(|k| k.0), Some("apercus"));
        assert_eq!(kind("vid_20260920_092259_00_581.INSV").map(|k| k.0), Some("originaux"));
        for bad in ["../VID_20260920_092259_00_581.insv", "VID_20260920_092259_00_581.lrv", "notes.txt", "LRV_2026_1.lrv"] {
            assert!(kind(bad).is_none(), "{bad}");
        }
    }
}
