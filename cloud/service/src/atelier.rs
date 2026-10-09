//! Atelier à la demande : un `bike360-server` par compte, lancé en mode hébergé quand le client
//! ouvre son atelier, arrêté quand il ne s'en sert plus. Ses clips et réglages sont déposés dans le
//! stockage à l'arrêt et repris à l'ouverture suivante.
//!
//! Ce lanceur démarre un processus sur la machine du service et recopie les aperçus du client sur
//! son disque. Sur AWS, le même rôle reviendra à une tâche Fargate dont le stockage est monté.
//!
//! Routes (JSON) :
//!   GET  /api/atelier          → {available, running}
//!   POST /api/atelier/ouvrir   → {url}   adresse à usage unique qui ouvre la session du client
//!   POST /api/atelier/fermer   → {ok}    enregistre puis arrête

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::account::Scope;
use crate::{Ctx, Fail};

/// Attente maximale du démarrage d'un atelier.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// Intervalle entre deux relevés d'activité.
const WATCH_EVERY: Duration = Duration::from_secs(30);

pub struct Launcher {
    /// Exécutable de l'atelier (bike360-server).
    pub bin: PathBuf,
    /// Dossier de travail : un sous-dossier par client.
    pub work: PathBuf,
    /// Nom ou adresse sous lequel le navigateur du client joint cette machine.
    pub host: String,
    /// Inactivité (s) au bout de laquelle un atelier est arrêté.
    pub idle_s: u64,
    running: Mutex<HashMap<String, Instance>>,
}

struct Instance {
    child: Child,
    port: u16,
    /// Mot de passe de l'atelier, connu du seul service.
    password: String,
}

fn random_hex() -> Result<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Appel à l'atelier d'un client, authentifié par son mot de passe.
async fn call(host: &str, port: u16, password: &str, method: &'static str, path: &'static str, body: Option<Value>) -> Result<Value> {
    let (url, auth) = (format!("http://{host}:{port}{path}"), format!("Bearer {password}"));
    tokio::task::spawn_blocking(move || -> Result<Value> {
        let req = ureq::request(method, &url).set("Authorization", &auth).timeout(Duration::from_secs(10));
        let res = match body {
            Some(b) => req.send_json(b)?,
            None => req.call()?,
        };
        Ok(res.into_json()?)
    }).await?
}

impl Launcher {
    pub fn new(bin: PathBuf, work: PathBuf, host: String, idle_s: u64) -> Launcher {
        Launcher { bin, work, host, idle_s, running: Mutex::new(HashMap::new()) }
    }

    fn dir(&self, client: &str) -> PathBuf {
        self.work.join(client)
    }
}

impl Ctx {
    /// Copie sur le disque les objets de `prefix` qui n'y sont pas déjà à la bonne taille.
    async fn pull(&self, prefix: &str, dir: &Path, keep: impl Fn(&str) -> bool) -> Result<()> {
        let mut token = None::<String>;
        loop {
            let page = self.s3.list_objects_v2().bucket(&self.bucket).prefix(prefix).set_continuation_token(token.take()).send().await?;
            for o in page.contents() {
                let (Some(key), Some(size)) = (o.key(), o.size()) else { continue };
                let rel = &key[prefix.len()..];
                // un nom d'objet devient un chemin : rien qui remonte hors du dossier
                if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") || !keep(rel) {
                    continue;
                }
                let to = dir.join(rel);
                if std::fs::metadata(&to).is_ok_and(|m| m.len() == size as u64) {
                    continue;
                }
                std::fs::create_dir_all(to.parent().unwrap_or(dir))?;
                let out = self.s3.get_object().bucket(&self.bucket).key(key).send().await?;
                tokio::io::copy(&mut out.body.into_async_read(), &mut tokio::fs::File::create(&to).await?).await?;
            }
            match page.next_continuation_token() {
                Some(next) => token = Some(next.to_string()),
                None => return Ok(()),
            }
        }
    }

    /// Dépose dans le stockage les fichiers de `dir` (récursivement) sous `prefix`.
    async fn push(&self, dir: &Path, prefix: &str) -> Result<usize> {
        let (mut stack, mut count) = (vec![dir.to_path_buf()], 0);
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(rel) = p.strip_prefix(dir) {
                    self.s3.put_object().bucket(&self.bucket).key(format!("{prefix}{}", rel.to_string_lossy()))
                        .body(ByteStream::from_path(&p).await?).send().await?;
                    count += 1;
                }
            }
        }
        Ok(count)
    }
}

impl Scope {
    fn launcher(&self) -> Result<&Launcher, Fail> {
        self.launcher.as_ref().ok_or_else(|| Fail(StatusCode::NOT_FOUND, "l'atelier n'est pas proposé par ce service".into()))
    }

    /// Démarre l'atelier du client sur ses aperçus, ses analyses et son travail enregistré.
    async fn start(&self, l: &Launcher) -> Result<Instance> {
        let dir = l.dir(&self.client);
        let (rushs, data, cache) = (dir.join("rushs"), dir.join("data"), dir.join("cache"));
        for d in [&rushs, &data, &cache, &dir.join("exports"), &dir.join("root")] {
            std::fs::create_dir_all(d)?;
        }
        let c = &self.client;
        self.pull(&format!("apercus/{c}/"), &rushs, |n| !n.contains('/')).await.context("copie des aperçus")?;
        self.pull(&format!("donnees/{c}/cache/"), &cache, |n| n.ends_with(".json") && !n.contains('/')).await?;
        self.pull(&format!("donnees/{c}/gps/"), &data.join("gps"), |n| n.ends_with(".gpx") && !n.contains('/')).await?;
        self.pull(&format!("donnees/{c}/atelier/"), &data, |_| true).await.context("reprise du travail enregistré")?;

        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let password = random_hex()?;
        let log = std::fs::File::create(dir.join("atelier.log"))?;
        let child = Command::new(&l.bin).arg(&rushs).args(["--host", &l.host, "--port", &port.to_string()])
            .env("BIKE360_CLOUD", "1").env("BIKE360_PASSWORD", &password)
            .env("BIKE360_ROOT", dir.join("root")).env("BIKE360_DATA", &data).env("BIKE360_CACHE", &cache)
            .env("BIKE360_EXPORTS", dir.join("exports")).env("HOME", &dir)
            .stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log)
            .kill_on_drop(true).spawn().context("lancement de l'atelier")?;
        let inst = Instance { child, port, password };
        let started = std::time::Instant::now();
        while call(&l.host, port, &inst.password, "GET", "/api/activite", None).await.is_err() {
            if started.elapsed() > START_TIMEOUT {
                bail!("l'atelier n'a pas démarré à temps (voir {})", dir.join("atelier.log").display());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(inst)
    }

    /// Enregistre le travail du client dans le stockage (clips, projet, réglages).
    async fn save(&self, l: &Launcher) -> Result<usize> {
        self.push(&l.dir(&self.client).join("data"), &format!("donnees/{}/atelier/", self.client)).await
    }

    /// Un aperçu vient d'arriver : il rejoint l'atelier du client s'il tourne, qui relance son analyse.
    pub async fn atelier_arrival(&self) {
        let Some(l) = &self.launcher else { return };
        let running = l.running.lock().await;
        let Some(inst) = running.get(&self.client) else { return };
        let done = async {
            self.pull(&format!("apercus/{}/", self.client), &l.dir(&self.client).join("rushs"), |n| !n.contains('/')).await?;
            call(&l.host, inst.port, &inst.password, "POST", "/api/sources", Some(json!({"rescan": true}))).await
        }.await;
        if let Err(e) = done {
            eprintln!("atelier de {} non prévenu : {e:#}", self.client);
        }
    }
}

fn failed(what: &'static str) -> impl FnOnce(anyhow::Error) -> Fail {
    move |e| {
        eprintln!("{what} : {e:#}");
        Fail(StatusCode::BAD_GATEWAY, format!("{what} impossible pour l'instant"))
    }
}

pub async fn status(c: Scope) -> Json<Value> {
    let running = match &c.launcher {
        Some(l) => l.running.lock().await.contains_key(&c.client),
        None => false,
    };
    Json(json!({"available": c.launcher.is_some(), "running": running}))
}

/// Ouvre l'atelier du client (en le démarrant s'il le faut) et renvoie une adresse à usage unique.
pub async fn open(c: Scope) -> Result<Json<Value>, Fail> {
    let l = c.launcher()?;
    let mut running = l.running.lock().await;
    let alive = match running.get_mut(&c.client) {
        Some(inst) => inst.child.try_wait().is_ok_and(|s| s.is_none()),
        None => false,
    };
    if !alive {
        running.remove(&c.client);
        let inst = c.start(l).await.map_err(failed("ouverture de l'atelier"))?;
        running.insert(c.client.clone(), inst);
    }
    let inst = &running[&c.client];
    let token = call(&l.host, inst.port, &inst.password, "POST", "/api/ouverture", Some(json!({}))).await.map_err(failed("ouverture de l'atelier"))?;
    let token = token["jeton"].as_str().ok_or_else(|| Fail(StatusCode::BAD_GATEWAY, "atelier sans jeton d'ouverture".into()))?;
    Ok(Json(json!({"url": format!("http://{}:{}/ouvrir?jeton={token}", l.host, inst.port)})))
}

/// Enregistre le travail du client puis arrête son atelier.
pub async fn close(c: Scope) -> Result<Json<Value>, Fail> {
    let l = c.launcher()?;
    let mut running = l.running.lock().await;
    let Some(mut inst) = running.remove(&c.client) else { return Ok(Json(json!({"ok": true, "saved": 0}))) };
    let saved = c.save(l).await.map_err(failed("enregistrement du travail"))?;
    let _ = inst.child.kill().await;
    Ok(Json(json!({"ok": true, "saved": saved})))
}

/// Arrêt du service : chaque atelier ouvert est enregistré puis arrêté.
pub async fn close_all(ctx: &Arc<Ctx>) {
    let Some(l) = &ctx.launcher else { return };
    for (client, mut inst) in l.running.lock().await.drain() {
        if let Err(e) = Scope::of(ctx.clone(), client.clone()).save(l).await {
            eprintln!("atelier de {client} arrêté sans enregistrement : {e:#}");
        }
        let _ = inst.child.kill().await;
    }
}

/// Surveille les ateliers : celui qui ne sert plus et ne calcule rien est enregistré puis arrêté.
pub async fn watch(ctx: Arc<Ctx>) {
    let Some(l) = &ctx.launcher else { return };
    loop {
        tokio::time::sleep(WATCH_EVERY).await;
        let clients: Vec<String> = l.running.lock().await.keys().cloned().collect();
        for client in clients {
            let mut running = l.running.lock().await;
            let Some(inst) = running.get_mut(&client) else { continue };
            let gone = !inst.child.try_wait().is_ok_and(|s| s.is_none());
            let idle = match call(&l.host, inst.port, &inst.password, "GET", "/api/activite", None).await {
                Ok(a) => a["idle_s"].as_u64().unwrap_or(0) > l.idle_s && a["busy"] == json!(false),
                Err(_) => false,
            };
            if !gone && !idle {
                continue;
            }
            // un atelier dont le travail n'a pas pu être enregistré reste ouvert, sauf s'il s'est arrêté seul
            match Scope::of(ctx.clone(), client.clone()).save(l).await {
                Ok(n) => println!("atelier de {client} arrêté ({n} fichiers enregistrés)"),
                Err(e) if gone => eprintln!("atelier de {client} arrêté sans enregistrement : {e:#}"),
                Err(e) => {
                    eprintln!("atelier de {client} : enregistrement en échec, il reste ouvert : {e:#}");
                    continue;
                }
            }
            if let Some(mut inst) = running.remove(&client) {
                let _ = inst.child.kill().await;
            }
        }
    }
}
