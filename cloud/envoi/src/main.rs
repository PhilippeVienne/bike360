//! Module Envoi : le navigateur dépose les rushs directement dans S3, par morceaux, et reprend
//! après une coupure. Ce service ne voit jamais passer les vidéos : il ouvre l'envoi, signe les
//! adresses des morceaux et assemble le fichier à la fin.
//!
//! Usage : bike360-envoi --bucket NOM [--client ID] [--ui DOSSIER] [--port 8370]
//! Routes (JSON) :
//!   POST /api/envoi/start    {name, size}            → {done} ou {upload_id, part_size, parts, received}
//!   POST /api/envoi/urls     {name, upload_id, parts} → {urls: {numéro: adresse signée}}
//!   POST /api/envoi/complete {name, size, upload_id}  → {ok, key}
//!   GET  /api/envoi/rushs                             → [{name, kind, size, class}]

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, StorageClass};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
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
    /// Client servi par cette instance (l'authentification viendra avec le portail)
    #[arg(long, env = "BIKE360_CLIENT", default_value = "demo")]
    client: String,
    /// Taille d'un morceau en Mo (5 au minimum, imposé par S3)
    #[arg(long, env = "BIKE360_PART_MB", default_value_t = 16)]
    part_mb: u64,
    /// Dossier de l'interface web à servir (page envoi.html)
    #[arg(long)]
    ui: Option<String>,
    #[arg(long, default_value_t = 8370)]
    port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

struct Ctx {
    s3: aws_sdk_s3::Client,
    bucket: String,
    client: String,
    part_size: u64,
}

/// Erreur renvoyée au navigateur : statut et message.
struct Fail(StatusCode, String);

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

impl Ctx {
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

#[derive(Deserialize)]
struct Start {
    name: String,
    size: u64,
}

/// Ouvre ou reprend l'envoi d'un rush. Un fichier déjà complet n'est pas renvoyé.
async fn start(State(c): State<Arc<Ctx>>, Json(b): Json<Start>) -> Result<Json<Value>, Fail> {
    let (key, class) = c.key(&b.name)?;
    let count = c.layout(b.size)?;
    if let Ok(head) = c.s3.head_object().bucket(&c.bucket).key(&key).send().await {
        if head.content_length() == Some(b.size as i64) {
            return Ok(Json(json!({"done": true, "key": key})));
        }
    }
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
async fn urls(State(c): State<Arc<Ctx>>, Json(b): Json<Urls>) -> Result<Json<Value>, Fail> {
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
async fn complete(State(c): State<Arc<Ctx>>, Json(b): Json<Complete>) -> Result<Json<Value>, Fail> {
    let (key, _) = c.key(&b.name)?;
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
    Ok(Json(json!({"ok": true, "key": key})))
}

/// Rushs du client déjà dans le stockage.
async fn rushs(State(c): State<Arc<Ctx>>) -> Result<Json<Value>, Fail> {
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
    let ctx = Arc::new(Ctx { s3, bucket: a.bucket, client: a.client, part_size: a.part_mb * 1024 * 1024 });
    let mut app = Router::new()
        .route("/api/envoi/start", post(start))
        .route("/api/envoi/urls", post(urls))
        .route("/api/envoi/complete", post(complete))
        .route("/api/envoi/rushs", get(rushs))
        .with_state(ctx.clone());
    if let Some(ui) = &a.ui {
        app = app.nest_service("/ui", tower_http::services::ServeDir::new(ui));
    }
    let listener = tokio::net::TcpListener::bind((a.host.as_str(), a.port)).await.context("ouverture du port")?;
    println!("Envoi → http://{}:{}/ui/envoi.html (compartiment {}, client {})", a.host, a.port, ctx.bucket, ctx.client);
    axum::serve(listener, app).await?;
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
