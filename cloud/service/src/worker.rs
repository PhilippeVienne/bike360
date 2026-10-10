//! Exécutant des tâches déposées dans la file. Une tâche « analyse » fait, pour une session qui
//! vient d'arriver : copie de ses aperçus sur le disque local, analyse et vignette (outil
//! `bike360-tool arrivee`), dépôt des résultats dans le stockage et résumé dans l'index, où la
//! Bibliothèque le lit. Une tâche qui échoue reste dans la file, qui la représente puis l'écarte après trois essais.
//!
//! Usage : bike360-worker --bucket NOM --table NOM --queue URL [--tool CHEMIN] [--work DOSSIER] [--once]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_s3::primitives::ByteStream;
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;

#[derive(Parser)]
#[command(about = "Exécute les tâches d'analyse de Bike360 Cloud")]
struct Args {
    #[arg(long, env = "BIKE360_BUCKET")]
    bucket: String,
    #[arg(long, env = "BIKE360_TABLE")]
    table: String,
    /// Adresse de la file des tâches
    #[arg(long, env = "BIKE360_QUEUE")]
    queue: String,
    /// Outil d'analyse du cœur
    #[arg(long, default_value = "bike360-tool")]
    tool: PathBuf,
    /// Dossier de travail local (effacé session par session)
    #[arg(long, default_value = "/tmp/bike360-worker")]
    work: PathBuf,
    /// Traite les tâches en attente puis s'arrête (au lieu d'attendre les suivantes)
    #[arg(long)]
    once: bool,
}

#[derive(Deserialize)]
struct Job {
    job: String,
    client: String,
    session: String,
}

struct Worker {
    s3: aws_sdk_s3::Client,
    db: aws_sdk_dynamodb::Client,
    args: Args,
}

/// Un identifiant venu d'un message devient un nom de dossier et un préfixe : lettres, chiffres, - et _ seulement.
fn safe(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

impl Worker {
    /// Noms des objets sous `prefix` (sans le préfixe).
    async fn names(&self, prefix: &str) -> Result<Vec<String>> {
        let (mut out, mut token) = (vec![], None::<String>);
        loop {
            let page = self.s3.list_objects_v2().bucket(&self.args.bucket).prefix(prefix).set_continuation_token(token.take()).send().await?;
            out.extend(page.contents().iter().filter_map(|o| o.key()?.strip_prefix(prefix).map(String::from)));
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_string()),
                None => return Ok(out),
            }
        }
    }

    async fn download(&self, key: &str, to: &Path) -> Result<()> {
        let out = self.s3.get_object().bucket(&self.args.bucket).key(key).send().await.with_context(|| format!("lecture de {key}"))?;
        let mut body = out.body.into_async_read();
        let mut file = tokio::fs::File::create(to).await?;
        tokio::io::copy(&mut body, &mut file).await?;
        Ok(())
    }

    async fn upload(&self, from: &Path, key: &str, content_type: &str) -> Result<()> {
        self.s3.put_object().bucket(&self.args.bucket).key(key).content_type(content_type)
            .body(ByteStream::from_path(from).await?).send().await.with_context(|| format!("dépôt de {key}"))?;
        Ok(())
    }

    /// Analyse d'arrivée d'une session : aperçus → analyse et vignette → stockage et index.
    async fn analyse(&self, client: &str, session: &str) -> Result<usize> {
        ensure!(safe(client) && safe(session) && session.len() >= 19, "identifiants de tâche invalides");
        let dir = self.args.work.join(client).join(session);
        let _ = std::fs::remove_dir_all(&dir);
        let (rushs, data, thumbs) = (dir.join("rushs"), dir.join("data"), dir.join("vignettes"));
        std::fs::create_dir_all(&rushs)?;
        std::fs::create_dir_all(data.join("gps"))?;

        // aperçus de la session (tous ses segments) ; le montage S3 Files rendra cette copie inutile
        let prefix = format!("apercus/{client}/");
        let wanted = format!("LRV_{}_", &session[4..19]);
        let files: Vec<String> = self.names(&prefix).await?.into_iter().filter(|n| n.starts_with(&wanted)).collect();
        ensure!(!files.is_empty(), "aucun aperçu pour {session}");
        for name in &files {
            self.download(&format!("{prefix}{name}"), &rushs.join(name)).await?;
        }
        // traces GPS du client : elles servent de source de positions à l'analyse
        let gps_prefix = format!("donnees/{client}/gps/");
        for name in self.names(&gps_prefix).await?.into_iter().filter(|n| n.ends_with(".gpx") && !n.contains('/')) {
            self.download(&format!("{gps_prefix}{name}"), &data.join("gps").join(&name)).await?;
        }

        // un processus par tâche : ses dossiers de données lui sont propres
        let (tool, rushs2, thumbs2, data2, home) = (self.args.tool.clone(), rushs.clone(), thumbs.clone(), data.clone(), dir.clone());
        let out = tokio::task::spawn_blocking(move || {
            Command::new(tool).arg("arrivee").arg(rushs2).arg(thumbs2).env("BIKE360_DATA", data2).env("HOME", home).output()
        }).await??;
        if !out.status.success() {
            bail!("analyse en échec : {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or_default());
        }
        let summary: Vec<Value> = serde_json::from_slice(&out.stdout).context("résumé d'analyse illisible")?;

        for s in &summary {
            let id = s["id"].as_str().context("résumé sans identifiant")?;
            let cache = data.join("cache").join(format!("{id}.json"));
            self.upload(&cache, &format!("donnees/{client}/cache/{id}.json"), "application/json").await?;
            let thumb_key = match s["thumb"].as_str() {
                Some(name) => {
                    let key = format!("donnees/{client}/vignettes/{name}");
                    self.upload(&thumbs.join(name), &key, "image/jpeg").await?;
                    Some(key)
                }
                None => None,
            };
            // la session analysée, puis ses parties (enregistrement en boucle) qui y renvoient
            let parts: Vec<&str> = s["parts"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            for part in std::iter::once(id).chain(parts.into_iter().filter(|p| *p != id)) {
                let st = |v: &str| AttributeValue::S(v.to_string());
                let num = |v: &Value| v.as_f64().map(|x| AttributeValue::N(x.to_string()));
                let mut put = self.db.put_item().table_name(&self.args.table)
                    .item("pk", st(&format!("client#{client}")))
                    .item("sk", st(&format!("session#{part}")))
                    .item("bloc", st(id))
                    .item("analyse", st(&chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()));
                for (attr, value) in [("duree_s", &s["duration_s"]), ("gps", &s["gps_coverage"]), ("candidats", &s["candidates"]),
                                      ("distance_km", &s["stats"]["distance_km"]), ("mobile_s", &s["stats"]["moving_s"]),
                                      ("vitesse_max", &s["stats"]["max_speed_kmh"])] {
                    if let Some(n) = num(value) {
                        put = put.item(attr, n);
                    }
                }
                if let Some(src) = s["gps_source"].as_str() {
                    put = put.item("gps_source", st(src));
                }
                if let Some(key) = &thumb_key {
                    put = put.item("vignette", st(key));
                }
                put.send().await.context("inscription du résumé")?;
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        Ok(summary.len())
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let conf = aws_config::load_from_env().await;
    let local = std::env::var("AWS_ENDPOINT_URL").is_ok_and(|v| !v.is_empty());
    let s3 = aws_sdk_s3::Client::from_conf(aws_sdk_s3::config::Builder::from(&conf).force_path_style(local).build());
    let sqs = aws_sdk_sqs::Client::new(&conf);
    let w = Worker { s3, db: aws_sdk_dynamodb::Client::new(&conf), args };
    println!("Exécutant prêt : file {}", w.args.queue);
    loop {
        let got = sqs.receive_message().queue_url(&w.args.queue).max_number_of_messages(1)
            .wait_time_seconds(if w.args.once { 1 } else { 20 }).send().await.context("lecture de la file")?;
        let Some(msg) = got.messages().first() else {
            if w.args.once {
                return Ok(());
            }
            continue;
        };
        let body = msg.body().unwrap_or_default();
        let done = match serde_json::from_str::<Job>(body) {
            Ok(j) if j.job == "analyse" => w.analyse(&j.client, &j.session).await.map(|n| format!("analyse de {} : {n} session(s)", j.session)),
            Ok(j) => Err(anyhow::anyhow!("tâche inconnue : {}", j.job)),
            Err(e) => Err(anyhow::anyhow!("message illisible : {e}")),
        };
        match done {
            Ok(text) => {
                println!("{text}");
                if let Some(handle) = msg.receipt_handle() {
                    sqs.delete_message().queue_url(&w.args.queue).receipt_handle(handle).send().await.context("retrait de la tâche")?;
                }
            }
            // la tâche reste dans la file : elle sera représentée, puis écartée après trois échecs
            Err(e) => eprintln!("échec : {e:#}"),
        }
    }
}
