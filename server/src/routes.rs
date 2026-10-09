//! Routes HTTP (mêmes chemins, mêmes JSON que server.py). Le traitement se fait hors de la
//! boucle asynchrone (fichiers, ffmpeg, Python) ; les fichiers sont servis en flux, avec les
//! requêtes Range (lecture vidéo et navigation sur téléphone).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bike360_core::{analyze, automontage, finishing, geometry, gpx, hyperlapse, insta360, musiclib, position, rides};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{audio, sources};
use crate::app::{self, exports_dir, music_dir, overrides_path, read_json, sources_path,
                 write_json_indent, App, Sess, MUSIC_EXT};
use crate::export::{self, float_of, int_of, HyperOpts, FORMATS, HEIGHTS};
use crate::{privacy, pyjson};
use bike360_core::privacy as core_privacy;

const MUSIC_MAX_BYTES: usize = 60 * 1024 * 1024;
const GPX_MAX_BYTES: usize = 60 * 1024 * 1024;

/// Réponse préparée par le traitement synchrone.
pub enum Reply {
    Json(u16, Value),
    File(PathBuf, Option<&'static str>),
    /// Fichier de l'interface embarqué dans le binaire.
    Static(&'static [u8], &'static str),
    Error(u16),
}

fn ok(v: Value) -> Reply {
    Reply::Json(200, v)
}
fn err(code: u16, msg: impl Into<String>) -> Reply {
    Reply::Json(code, json!({"error": msg.into()}))
}

pub async fn dispatch(State(app): State<Arc<App>>, ConnectInfo(addr): ConnectInfo<SocketAddr>, method: Method, uri: Uri,
                      headers: HeaderMap, body: Bytes) -> Response {
    let full = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_else(|| uri.path().to_string());
    if crate::auth::enabled() {
        if let Some(resp) = gate(&uri, &method, &headers, &body, addr.ip()).await {
            return resp;
        }
    }
    let h = headers.clone();
    let m = method.clone();
    let f = full.clone();
    let reply = tokio::task::spawn_blocking(move || handle(&app, &m, &f, &h, &body))
        .await
        .unwrap_or(Reply::Error(500));
    let resp = match reply {
        Reply::Json(code, v) => {
            let body = serde_json::to_vec(&v).unwrap_or_default();
            Response::builder()
                .status(code)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap()
        }
        Reply::File(path, ctype) => serve_file(&path, ctype, &headers).await,
        Reply::Static(data, ctype) => Response::builder()
            .status(200)
            .header(header::CONTENT_TYPE, ctype)
            .header(header::CONTENT_LENGTH, data.len())
            .body(Body::from(data))
            .unwrap(),
        Reply::Error(code) => error_page(code),
    };
    if !uri.path().starts_with("/media/") && !uri.path().starts_with("/api/export") {
        eprintln!("{} - - \"{} {}\" {}", addr.ip(), method, full, resp.status().as_u16());
    }
    resp
}

/// Contrôle d'accès : page de connexion, déconnexion, refus des requêtes sans session.
/// None = requête autorisée, à traiter normalement.
async fn gate(uri: &Uri, method: &Method, headers: &HeaderMap, body: &Bytes, ip: std::net::IpAddr) -> Option<Response> {
    use crate::auth;
    let page = |code: u16, html: String, cookie: Option<String>| {
        let mut r = Response::builder().status(code).header(header::CONTENT_TYPE, "text/html; charset=utf-8").header(header::CACHE_CONTROL, "no-store");
        if let Some(c) = cookie {
            r = r.header(header::SET_COOKIE, c);
        }
        r.body(Body::from(html)).unwrap()
    };
    let https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let redirect = |to: &str, cookie: Option<String>| {
        let mut r = Response::builder().status(303).header(header::LOCATION, to).header(header::CACHE_CONTROL, "no-store");
        if let Some(c) = cookie {
            r = r.header(header::SET_COOKIE, c);
        }
        r.body(Body::empty()).unwrap()
    };
    let query_next = || {
        let q = uri.query().unwrap_or("");
        auth::safe_next(&auth::form_field(q, "next"))
    };
    match uri.path() {
        "/login" if method == Method::POST => {
            let text = String::from_utf8_lossy(body).into_owned();
            let next = auth::safe_next(&auth::form_field(&text, "next"));
            if auth::blocked(ip) {
                return Some(page(429, auth::login_page("Trop d'essais : réessaie dans quelques minutes.", &next), None));
            }
            match auth::login(&auth::form_field(&text, "password")) {
                Some(tok) => Some(redirect(&next, Some(auth::set_cookie(&tok, https)))),
                None => {
                    auth::record_fail(ip);
                    let _ = tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_secs(1))).await;   // ralentit les essais
                    Some(page(401, auth::login_page("Mot de passe incorrect.", &next), None))
                }
            }
        }
        "/login" => Some(if auth::authorized(headers) { redirect(&query_next(), None) } else { page(200, auth::login_page("", &query_next()), None) }),
        "/logout" => Some(redirect("/login", Some(auth::set_cookie("", https)))),
        _ if auth::authorized(headers) => None,
        p if p.starts_with("/api/") => Some(json_401()),
        _ if ["/media/", "/music/", "/exports/", "/thumb/", "/minimap"].iter().any(|x| uri.path().starts_with(x)) => Some(error_page(401)),
        _ => {
            let to = uri.path_and_query().map_or("/".to_string(), |p| p.as_str().to_string());
            let enc: String = to.bytes().map(|b| if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
            Some(redirect(&format!("/login?next={enc}"), None))
        }
    }
}

fn json_401() -> Response {
    let body = serde_json::to_vec(&json!({"error": "authentification requise"})).unwrap_or_default();
    Response::builder().status(401).header(header::CONTENT_TYPE, "application/json").body(Body::from(body)).unwrap()
}

fn error_page(code: u16) -> Response {
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let msg = format!("Error {code}: {}\n", status.canonical_reason().unwrap_or(""));
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], msg).into_response()
}

fn mime(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("html" | "htm") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/vnd.microsoft.icon",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("txt") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Sert un fichier avec support des requêtes Range.
async fn serve_file(path: &Path, ctype: Option<&'static str>, headers: &HeaderMap) -> Response {
    let Ok(mut file) = tokio::fs::File::open(path).await else { return error_page(404) };
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return error_page(404),
    };
    let ctype = ctype.unwrap_or_else(|| mime(path));
    let (mut start, mut end) = (0u64, size.saturating_sub(1));
    let mut partial = false;
    if let Some(r) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|r| r.strip_prefix("bytes=")) {
        let (a, b) = r.split_once('-').unwrap_or((r, ""));
        let b = b.split(',').next().unwrap_or("");
        let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
        if digits(a) && digits(b) && !(a.is_empty() && b.is_empty()) {
            if !a.is_empty() {
                start = a.parse().unwrap_or(0);
                if !b.is_empty() {
                    end = b.parse().unwrap_or(end);
                }
            } else {
                start = size.saturating_sub(b.parse().unwrap_or(0));
            }
            end = end.min(size.saturating_sub(1));
            partial = true;
        }
    }
    if partial && (start > end || start >= size) {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .unwrap();
    }
    let len = if size == 0 { 0 } else { end - start + 1 };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return error_page(500);
    }
    let stream = tokio_util::io::ReaderStream::with_capacity(file.take(len), 1 << 20);
    let mut b = Response::builder()
        .status(if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header(header::CONTENT_TYPE, ctype)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len);
    if partial {
        b = b.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    b.body(Body::from_stream(stream)).unwrap()
}

// ---------------------------------------------------------------- outils

/// Paramètres d'URL comme la version Python (`dict(x.split("=", 1) …)`, sans décodage).
fn raw_query(full: &str) -> Map<String, Value> {
    let q = full.split_once('?').map(|(_, q)| q).unwrap_or("");
    q.split('&').filter_map(|x| x.split_once('=')).map(|(k, v)| (k.to_string(), Value::from(v))).collect()
}

/// Décodage d'URL (%xx, et « + » en espace pour les formulaires).
fn unquote(s: &str, plus: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_qs(full: &str) -> Map<String, Value> {
    let q = full.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut out = Map::new();
    for x in q.split('&').filter(|x| !x.is_empty()) {
        let (k, v) = x.split_once('=').unwrap_or((x, ""));
        if v.is_empty() {
            continue; // parse_qs ignore les valeurs vides
        }
        let k = unquote(k, true);
        if !out.contains_key(&k) {
            out.insert(k, unquote(v, true).into());
        }
    }
    out
}

fn body_json(body: &Bytes) -> Result<Value, ()> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(body).map_err(|_| ())
}

/// `self._body() or {}` : objet JSON du corps (vide si absent ou nul).
fn body_obj(body: &Bytes) -> Result<Value, ()> {
    Ok(match body_json(body)? {
        Value::Null => json!({}),
        Value::Object(m) if m.is_empty() => json!({}),
        v => v,
    })
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn clip_id(c: &Map<String, Value>) -> String {
    c.get("id").map(pyjson::py_str).unwrap_or_default()
}

fn find_clip(app: &App, sid: Option<&str>, id: Option<&Value>) -> Option<Map<String, Value>> {
    let sid = sid?;
    if !app.has(sid) {
        return None;
    }
    let id = id?;
    app.get_selections(sid).into_iter().find(|c| c.get("id") == Some(id))
}

fn job_json(app: &App, key: &str) -> Value {
    app.job(key).map(|j| j.snapshot()).unwrap_or(json!({"state": "idle"}))
}

fn result_json(s: &Sess) -> Map<String, Value> {
    let mut m = match serde_json::to_value(&s.result) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let pos = session_position(s);
    m.insert("front_yaw".into(), position::front_yaw(&pos).into());
    m.insert("position".into(), pos.into());
    m
}

/// Position de la caméra pour cette session (choix de la session, sinon de sa caméra, sinon guidon).
fn session_position(s: &Sess) -> String {
    let store = read_json(&position::path()).unwrap_or(Value::Null);
    position::resolve(&store, &s.result.id, s.session.camera.as_ref().map(|c| c.serial.as_str())).to_string()
}

// ---------------------------------------------------------------- aiguillage

fn handle(app: &Arc<App>, method: &Method, full: &str, headers: &HeaderMap, body: &Bytes) -> Reply {
    let r = match *method {
        Method::GET => get(app, full),
        Method::PUT => put(app, full, body),
        Method::POST => post(app, full, headers, body),
        _ => Err(()),
    };
    r.unwrap_or(Reply::Error(400))
}

fn split(path: &str) -> Vec<String> {
    path.trim_matches('/').split('/').map(|s| s.to_string()).collect()
}

fn get(app: &Arc<App>, full: &str) -> Result<Reply, ()> {
    let path = full.split('?').next().unwrap_or("");
    let parts = split(path);
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    // interface : fichier sur disque (<racine>/ui) en priorité, sinon copie embarquée
    let ui_rel = if path == "/" {
        Some("index.html".to_string())
    } else if p[0] == "ui" && p.len() >= 2 {
        Some(p[1..].iter().map(|x| unquote(x, false)).collect::<Vec<_>>().join("/"))
    } else {
        None
    };
    if let Some(rel) = ui_rel {
        match app::ui_file(&rel) {
            Some(app::UiFile::Disk(f)) => return Ok(Reply::File(f, None)),
            Some(app::UiFile::Embedded(data)) => return Ok(Reply::Static(data, mime(Path::new(&rel)))),
            None => {}
        }
    }
    if p[0] == "media" && p.len() == 2 {
        let name = unquote(p[1], false);
        for s in app.sessions.read().unwrap().values() {
            for seg in &s.session.segments {
                if let Some(lrv) = &seg.lrv {
                    if lrv.file_name().is_some_and(|n| n.to_string_lossy() == name) {
                        return Ok(Reply::File(lrv.clone(), Some("video/mp4")));
                    }
                }
            }
        }
        return Ok(Reply::Error(404));
    }
    if p[0] == "exports" && p.len() == 2 {
        let name = unquote(p[1], false);
        let f = exports_dir().join(&name);
        if f.is_file() && !name.contains('/') {
            return Ok(Reply::File(f, None));
        }
    }
    if p[0] == "music" && p.len() == 2 {   // fichier audio du montage (écoute dans l'éditeur)
        return Ok(match audio::path_of(&unquote(p[1], false)) {
            Some(f) => Reply::File(f, None),
            None => Reply::Error(404),
        });
    }
    match path {
        "/api/sessions" => {
            let sessions = app.sessions.read().unwrap();
            let store = read_json(&position::path()).unwrap_or(Value::Null);
            let serial = |s: &Sess| s.session.camera.as_ref().map(|c| c.serial.clone());
            let serials: Vec<Option<String>> = sessions.values().map(|s| serial(s)).collect();
            let spans: Vec<rides::Span> = sessions.values().zip(&serials).map(|(s, cam)| {
                let start = s.result.utc_t0 + s.result.offset_s;
                rides::Span { id: &s.result.id, start, end: start + s.result.duration as f64, camera: cam.as_deref() }
            }).collect();
            let groups = rides::group(&spans);
            let list: Vec<Value> = sessions.iter().zip(&groups).map(|((sid, s), (ride, angles))| {
                let r = &s.result;
                let pos = position::resolve(&store, sid, s.session.camera.as_ref().map(|c| c.serial.as_str()));
                let clips = app.get_selections(sid);
                let clips_s: f64 = clips.iter().map(|c| float_of(c.get("end"), 0.0) - float_of(c.get("start"), 0.0)).sum();
                json!({"id": r.id, "date": r.date, "time": r.time, "duration": r.duration,
                       "gps_coverage": r.gps_coverage, "candidates": r.candidates.len(), "clips": clips.len(),
                       "clips_s": bike360_core::numeric::round_nd(clips_s, 1),
                       "folder": r.extra.get("folder").cloned().unwrap_or(Value::Null),
                       "parts": r.extra.get("parts").cloned().unwrap_or(json!(1)),
                       "camera": s.session.camera, "gps_source": r.gps_source,
                       "position": pos, "front_yaw": position::front_yaw(pos), "ride": ride, "angles": angles})
            }).collect();
            return Ok(ok(Value::Array(list)));
        }
        "/api/project" => {
            let proj = app.get_project();
            let items = app.montage_items_all(Some(&proj));
            let sessions = app.sessions.read().unwrap();
            let clips: Vec<Value> = items.iter().map(|(sid, c, ex)| {
                json!({"sid": sid, "id": c.get("id"), "start": c.get("start"), "end": c.get("end"), "excluded": ex,
                       "auto": truthy(c.get("auto")), "yaw": c.get("yaw").cloned().unwrap_or(json!(0)),
                       "pitch": c.get("pitch").cloned().unwrap_or(json!(0)),
                       "fov": c.get("fov").cloned().unwrap_or(json!(100)),
                       "speed_keys": c.get("speed_keys").cloned().unwrap_or(json!([])),
                       "utc": sessions.get(sid).map_or(0.0, |s| s.result.utc_t0) + float_of(c.get("start"), 0.0)})
            }).collect();
            let mut out = proj.as_object().cloned().unwrap_or_default();
            out.insert("clips".into(), Value::Array(clips));
            return Ok(ok(Value::Object(out)));
        }
        "/minimap.png" => {
            let qs = raw_query(full);
            let Some(sid) = qs.get("sid").and_then(Value::as_str).filter(|s| app.has(s)) else {
                return Ok(Reply::Error(404));
            };
            let size = qs.get("size").and_then(Value::as_str).map(|s| s.parse::<i64>()).unwrap_or(Ok(320));
            let Ok(size) = size else { return Ok(Reply::Error(500)) };
            return Ok(match export::minimap(app, sid, qs.get("clip").and_then(Value::as_str), size.clamp(160, 800) as u32) {
                Ok(img) => Reply::File(img, Some("image/png")),
                Err(e) => {
                    eprintln!("Mini-carte : {e:#}");
                    Reply::Error(500)
                }
            });
        }
        "/api/privacy" => {
            return Ok(ok(json!({"job": job_json(app, "privacy"),
                                "enabled": app.get_settings()["privacy"]["enabled"],
                                "clips": privacy_overview(app)})));
        }
        "/api/music" => return Ok(ok(json!(app.music_files()))),
        "/api/music/files" => return Ok(ok(Value::Array(audio::list(app)))),
        "/api/music/peaks" => {
            let name = unquote(raw_query(full).get("name").and_then(Value::as_str).unwrap_or(""), false);
            return Ok(match audio::peaks(&name) {
                Ok(v) => ok(json!({"per_s": audio::PEAKS_PER_S, "peaks": v.iter().map(|x| (x * 1000.0).round() / 1000.0).collect::<Vec<_>>()})),
                Err(e) => err(404, format!("{e:#}")),
            });
        }
        "/api/music/library" => {
            let qs = parse_qs(full);
            let s = |k: &str| qs.get(k).and_then(Value::as_str).unwrap_or("").to_string();
            return Ok(match musiclib::search(&s("q"), &s("mood"), 60, 60) {
                Ok(pieces) => {
                    let moods: Map<String, Value> = musiclib::MOODS.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
                    ok(json!({"moods": moods, "credits": musiclib::credits(), "pieces": pieces}))
                }
                Err(e) => err(502, format!("catalogue indisponible : {e}")),
            });
        }
        "/api/sources/detect" => return Ok(ok(Value::Array(sources::detect(app)))),
        "/api/fs" => {
            let dir = unquote(raw_query(full).get("path").and_then(Value::as_str).unwrap_or(""), false);
            return Ok(match sources::browse(&dir) {
                Ok(v) => ok(v),
                Err(e) => err(400, format!("{e:#}")),
            });
        }
        "/api/fs/preview" => {
            let dir = unquote(raw_query(full).get("path").and_then(Value::as_str).unwrap_or(""), false);
            return Ok(match sources::preview(app, &dir) {
                Ok(v) => ok(v),
                Err(e) => err(400, format!("{e:#}")),
            });
        }
        "/api/sources" => {
            let sessions = app.sessions.read().unwrap();
            let folders: Vec<Value> = app.source_folders().into_iter().map(|f| {
                let n = sessions.values().filter(|s| s.result.extra.get("folder").and_then(Value::as_str) == Some(&f)).count();
                json!({"path": f, "present": Path::new(&f).is_dir(), "removable": f != app.dcim, "sessions": n})
            }).collect();
            return Ok(ok(json!({"scan": app.scan.lock().unwrap().clone(), "folders": folders})));
        }
        "/api/gps" => return Ok(ok(Value::Array(gps_files()))),
        "/api/positions" => return Ok(ok(position::list())),
        "/api/settings" => return Ok(ok(app.get_settings())),
        _ => {}
    }
    if p[0] == "thumb" && p.len() == 2 {
        let sid = p[1].strip_suffix(".jpg").unwrap_or(p[1]);
        if let Some(s) = app.sess(sid) {
            let qs = raw_query(full);
            let num = |k: &str, d: f64| -> Option<f64> {
                match qs.get(k).and_then(Value::as_str) {
                    Some(v) => py_float(v),
                    None => Some(d),
                }
            };
            let front = position::front_yaw(&session_position(&s));
            let vals = (num("t", s.result.duration as f64 / 3.0), num("yaw", front), num("pitch", -10.0), num("fov", 100.0));
            let (Some(t), Some(yaw), Some(pitch), Some(fov)) = vals else { return Ok(Reply::Error(500)) };
            // largeur facultative (w=…, 160 à 1280 px) pour les illustrations ; 320 px par défaut
            let width = num("w", 320.0).unwrap_or(320.0).clamp(160.0, 1280.0) as u32;
            return Ok(match export::thumbnail(app, sid, t, yaw, pitch, fov.clamp(30.0, 150.0), width) {
                Ok(img) => Reply::File(img, Some("image/jpeg")),
                Err(e) => {
                    eprintln!("Vignette : {e:#}");
                    Reply::Error(500)
                }
            });
        }
    }
    if p.len() == 3 && p[0] == "api" {
        let sid = p[2];
        match p[1] {
            "privacy" if app.has(sid) => {
                let data = privacy::load(sid);
                let out: Map<String, Value> = data.iter().map(|(cid, e)| {
                    let tracks = core_privacy::all_tracks(Some(e)).into_iter().filter(|t| !t.samples.is_empty());
                    (cid.clone(), json!(tracks.collect::<Vec<_>>()))
                }).collect();
                return Ok(ok(Value::Object(out)));
            }
            "session" => {
                if let Some(s) = app.sess(sid) {
                    let mut r = result_json(&s);
                    // clips illisibles : erreur plutôt qu'une liste vide que l'interface réenregistrerait
                    let Ok(clips) = app.try_selections(sid) else { return Ok(err(503, "clips momentanément illisibles, réessaie")) };
                    r.insert("selections".into(), Value::Array(clips.into_iter().map(Value::Object).collect()));
                    return Ok(ok(Value::Object(r)));
                }
            }
            "horizon" if app.has(sid) => {
                let h = app.request_horizon(sid, true);
                let mut out = h.to_json();
                if h.status == "done" {
                    if let Some(d) = &h.data {
                        out["hz"] = json!(d.hz);
                        out["up"] = json!(d.up);
                        out["reliable"] = Value::Null;
                    }
                }
                return Ok(ok(out));
            }
            "export" => return Ok(ok(job_json(app, sid))),
            _ => {}
        }
    }
    if p[0] == "privacy-thumb" && p.len() == 2 {
        let name = unquote(p[1], false);
        let f = core_privacy::thumbs_dir().join(&name);
        if f.is_file() && !name.contains('/') {
            return Ok(Reply::File(f, Some("image/jpeg")));
        }
    }
    Ok(Reply::Error(404))
}

/// `float(texte)` de Python (accepte nan, inf, espaces).
fn py_float(s: &str) -> Option<f64> {
    let t = s.trim().to_lowercase();
    match t.as_str() {
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => t.replace('_', "").parse().ok(),
    }
}

/// État de l'analyse de confidentialité pour chaque clip du montage (revue).
fn privacy_overview(app: &App) -> Vec<Value> {
    app.montage_items().into_iter().map(|(sid, clip)| {
        let data = privacy::load(&sid);
        let entry = data.get(&clip_id(&clip));
        let tracks = core_privacy::all_tracks(entry);
        let analyzed = entry.and_then(|e| e.key.as_ref()).is_some();
        let stale = analyzed && !core_privacy::is_analyzed(&data, &Value::Object(clip.clone()));
        let tr: Vec<Value> = tracks.iter().filter(|t| !t.samples.is_empty()).map(|t| {
            let s = &t.samples;
            json!({"id": t.id, "kind": t.kind, "conf": t.conf, "enabled": t.enabled, "thumb": t.thumb,
                   "t0": s[0][0], "t1": s[s.len() - 1][0]})
        }).collect();
        json!({"sid": sid, "clip": clip.get("id"), "start": clip.get("start"), "end": clip.get("end"),
               "analyzed": analyzed, "stale": stale, "tracks": tr})
    }).collect()
}

fn put(app: &Arc<App>, full: &str, body: &Bytes) -> Result<Reply, ()> {
    let path = full.split('?').next().unwrap_or("");
    let parts = split(path);
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    if p == ["api", "settings"] {
        let cfg = body_obj(body)?;
        let current = app.get_settings();
        let masks_src = cfg.get("masks").cloned().unwrap_or(current["masks"].clone());
        let mut masks = vec![];
        for m in masks_src.as_array().ok_or(())? {
            let mut o = Map::new();
            for k in ["x", "y", "w", "h"] {
                let v = m.get(k).ok_or(())?;
                let f = match v {
                    Value::String(s) => py_float(s).ok_or(())?,
                    Value::Number(n) => n.as_f64().ok_or(())?,
                    Value::Bool(b) => *b as i64 as f64,
                    _ => return Err(()),
                };
                o.insert(k.into(), json!(f.clamp(0.0, 1.0)));
            }
            masks.push(Value::Object(o));
        }
        masks.truncate(8);
        let defaults = app::telemetry_defaults();
        let mut merged = current["telemetry"].as_object().cloned().unwrap_or_default();
        if let Some(Value::Object(t)) = cfg.get("telemetry") {
            for (k, v) in t {
                merged.insert(k.clone(), v.clone());
            }
        }
        let tel: Map<String, Value> = merged.into_iter().filter(|(k, _)| defaults.contains_key(k))
            .map(|(k, v)| (k, json!(truthy(Some(&v))))).collect();
        let mut priv_ = current["privacy"].as_object().cloned().unwrap_or_default();
        if let Some(Value::Object(pv)) = cfg.get("privacy") {
            for (k, v) in pv {
                priv_.insert(k.clone(), v.clone());
            }
        }
        let privacy = json!({"enabled": truthy(priv_.get("enabled"))});
        write_json_indent(&app::settings_path(), &json!({"masks": masks, "telemetry": tel, "privacy": privacy}))
            .map_err(|_| ())?;
        return Ok(ok(json!({"ok": true})));
    }
    if p.len() == 4 && p[0] == "api" && p[1] == "privacy" && app.has(p[2]) {
        let b = body_obj(body)?;
        // lecture stricte : un fichier illisible ne doit pas être écrasé par un fichier vide
        let mut data = core_privacy::load(p[2]).map_err(|_| ())?;
        let mut scratch = core_privacy::ClipEntry::default();
        let entry = match data.get_mut(p[3]) {
            Some(e) => e,
            None => &mut scratch,
        };
        let track = b.get("track").cloned().unwrap_or(Value::Null);
        if truthy(b.get("delete")) {
            // seules les zones tracées à la main se suppriment
            let mut manual = entry.manual.take().unwrap_or_default();
            manual.retain(|t| t.id != track);
            entry.manual = Some(manual);
        }
        let enabled = truthy(b.get("enabled"));
        for list in [&mut entry.tracks, &mut entry.manual].into_iter().flatten() {
            for t in list.iter_mut() {
                if t.id == track || track == json!("all") {
                    t.enabled = Some(enabled);
                }
            }
        }
        core_privacy::save(p[2], &data).map_err(|_| ())?;
        return Ok(ok(json!({"ok": true})));
    }
    if p == ["api", "project"] {
        let b = body_obj(body)?;
        let mut proj = app.get_project();
        // get_project() écarte les sessions absentes (carte retirée) : on garde la liste enregistrée,
        // sinon elles disparaîtraient du projet à la première modification
        if let Some(saved) = read_json(&app::project_path()).and_then(|p| p.get("sessions").cloned()).filter(Value::is_array) {
            proj["sessions"] = saved;
        }
        for k in ["sessions", "order", "excluded"] {
            if let Some(Value::Array(a)) = b.get(k) {
                proj[k] = Value::Array(a.clone());
            }
        }
        if let Some(Value::Object(st)) = b.get("style") {
            let mut s = proj["style"].as_object().cloned().unwrap_or_default();
            for (k, v) in st {
                s.insert(k.clone(), v.clone());
            }
            proj["style"] = finishing::clean(Some(&Value::Object(s))).map_err(|_| ())?;
        }
        write_json_indent(&app::project_path(), &proj).map_err(|_| ())?;
        return Ok(ok(json!({"ok": true})));
    }
    if p.len() == 3 && p[0] == "api" && p[1] == "selections" && app.has(p[2]) {
        let Value::Array(list) = body_json(body)? else { return Err(()) };
        let mut clips = vec![];
        for c in list {
            let Value::Object(mut c) = c else { return Err(()) };
            c.entry("id").or_insert_with(|| automontage::new_id().into());
            clips.push(c);
        }
        app.write_selections(p[2], &clips).map_err(|_| ())?;
        return Ok(ok(json!({"ok": true, "count": clips.len()})));
    }
    Ok(Reply::Error(404))
}

fn spawn(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(f);
}

/// Vue (écran → caméra) et boîte tracée dans l'aperçu → direction et demi-angles sur la sphère.
fn zone_from_body(b: &Value) -> Option<([f64; 3], f64, f64, Map<String, Value>)> {
    let v = b.get("view")?;
    let f = |x: &Value| -> Option<f64> {
        match x {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => py_float(s),
            _ => None,
        }
    };
    let level: Vec<f64> = v.get("level")?.as_array()?.iter().flat_map(|x| {
        x.as_array().map(|r| r.iter().map(f).collect::<Vec<_>>()).unwrap_or_else(|| vec![f(x)])
    }).collect::<Option<Vec<f64>>>()?;
    if level.len() != 9 {
        return None;
    }
    let l = [[level[0], level[1], level[2]], [level[3], level[4], level[5]], [level[6], level[7], level[8]]];
    let roll = match v.get("roll") {
        Some(x) => f(x)?,
        None => 0.0,
    };
    let m = geometry::view_matrix(f(v.get("yaw")?)?, f(v.get("pitch")?)?, Some(&l), roll);
    let bx: Vec<f64> = b.get("box")?.as_array()?.iter().map(f).collect::<Option<_>>()?;
    if bx.len() != 4 {
        return None;
    }
    let (w, h) = (1000.0 * f(b.get("aspect")?)?, 1000.0);
    let (d, ax, ay) = core_privacy::box_to_sphere(&[bx[0] * w, bx[1] * h, bx[2] * w, bx[3] * h], &m, f(v.get("fov")?)?, w, h);
    let mut view0 = Map::new();
    for k in ["yaw", "pitch", "roll", "fov"] {
        view0.insert(k.into(), json!(match v.get(k) {
            Some(x) => f(x)?,
            None => 0.0,
        }));
    }
    Some((d, ax, ay, view0))
}

fn post(app: &Arc<App>, full: &str, headers: &HeaderMap, body: &Bytes) -> Result<Reply, ()> {
    let path = full.split('?').next().unwrap_or("");
    let parts = split(path);
    let p: Vec<&str> = parts.iter().map(String::as_str).collect();
    if p == ["api", "sources"] {
        return sources(app, &body_obj(body)?);
    }
    if p == ["api", "music", "library"] {
        let b = body_obj(body)?;
        return Ok(match musiclib::download(&pyjson::py_str(b.get("filename").unwrap_or(&json!("")))) {
            Ok(name) => ok(json!({"ok": true, "name": name})),
            Err(e) => err(502, format!("téléchargement impossible : {e}")),
        });
    }
    if p == ["api", "music"] {
        return Ok(music_upload(full, headers, body));
    }
    if p == ["api", "gps"] {
        return Ok(gps_upload(app, full, headers, body));
    }
    if p == ["api", "position"] {
        // position de la caméra pour une session ; devient aussi le réglage par défaut de cette caméra
        let b = body_obj(body)?;
        let (Some(sid), Some(key)) = (b.get("sid").and_then(Value::as_str), b.get("position").and_then(Value::as_str)) else { return Ok(Reply::Error(400)) };
        let Some(s) = app.sess(sid) else { return Ok(Reply::Error(404)) };
        if !position::is_known(key) {
            return Ok(err(400, "position inconnue"));
        }
        let _g = app.lock.lock().unwrap();
        let mut store = read_json(&position::path()).unwrap_or(Value::Null);
        position::assign(&mut store, sid, s.session.camera.as_ref().map(|c| c.serial.as_str()), key);
        write_json_indent(&position::path(), &store).map_err(|_| ())?;
        return Ok(ok(json!({"ok": true})));
    }
    if p == ["api", "montage"] {
        let b = body_obj(body)?;
        let items = app.montage_items();
        if items.is_empty() {
            return Ok(err(400, "aucun clip dans le montage"));
        }
        let quality = b.get("quality").and_then(Value::as_str).unwrap_or("final").to_string();
        if !["preview", "final"].contains(&quality.as_str()) {
            return Ok(Reply::Error(400));
        }
        if app.job_running("montage") {
            return Ok(err(409, "montage déjà en cours"));
        }
        let opts = export::export_opts(&b, &quality);
        let format = opts.get("format").cloned().unwrap_or(json!("standard"));
        let job = app.start_job("montage", json!({"state": "running", "progress": 0.0, "quality": quality,
                                                  "message": "démarrage", "format": format}));
        let app2 = app.clone();
        spawn(move || export::run_export(app2, job, "montage".into(), items, quality, opts));
        return Ok(ok(json!({"ok": true})));
    }
    if p == ["api", "automontage"] {
        return automontage_route(app, &body_obj(body)?);
    }
    if p == ["api", "follow"] {
        let b = body_obj(body)?;
        let sid = b.get("sid").and_then(Value::as_str);
        let Some(clip) = find_clip(app, sid, b.get("clip")) else {
            return Ok(err(400, "place la tête de lecture dans un clip"));
        };
        if app.job_running("follow") {
            return Ok(err(409, "un suivi est déjà en cours"));
        }
        let (Some((d, ax, ay, view0)), Some(t)) = (zone_from_body(&b), b.get("t").map(|x| float_of(Some(x), f64::NAN)))
        else {
            return Ok(err(400, "zone invalide"));
        };
        let job = app.start_job("follow", json!({"state": "running", "progress": 0.0, "message": "démarrage du suivi"}));
        let (app2, sid, cid) = (app.clone(), sid.unwrap().to_string(), clip_id(&clip));
        spawn(move || export::run_follow(app2, job, sid, cid, t, d, ax, ay, view0));
        return Ok(ok(json!({"ok": true})));
    }
    if p == ["api", "privacy", "manual"] {
        let b = body_obj(body)?;
        let sid = b.get("sid").and_then(Value::as_str);
        let Some(clip) = find_clip(app, sid, b.get("clip")) else {
            return Ok(err(400, "place la tête de lecture dans un clip"));
        };
        if app.job_running("privacy") {
            return Ok(err(409, "une analyse est déjà en cours"));
        }
        let (Some((d, ax, ay, _)), Some(t)) = (zone_from_body(&b), b.get("t").map(|x| float_of(Some(x), f64::NAN)))
        else {
            return Ok(err(400, "zone invalide"));
        };
        let track_it = b.get("follow").map(|v| truthy(Some(v))).unwrap_or(true);
        let job = app.start_job("privacy", json!({"state": "running", "progress": 0.0, "message": "zone tracée"}));
        let (app2, sid) = (app.clone(), sid.unwrap().to_string());
        spawn(move || export::run_manual_zone(app2, job, sid, clip, t, d, ax, ay, track_it));
        return Ok(ok(json!({"ok": true})));
    }
    if p == ["api", "privacy", "analyze"] {
        let b = body_obj(body)?;
        if app.job_running("privacy") {
            return Ok(err(409, "analyse déjà en cours"));
        }
        let sid = b.get("sid").and_then(Value::as_str).filter(|s| app.has(s));
        let items: Vec<(String, Map<String, Value>)> = match sid {
            Some(sid) if truthy(b.get("clip")) => app.get_selections(sid).into_iter()
                .filter(|c| c.get("id") == b.get("clip"))
                .map(|c| (sid.to_string(), c))
                .collect(),
            _ => app.montage_items(),
        };
        if items.is_empty() {
            return Ok(err(400, "aucun clip à analyser"));
        }
        let force = truthy(b.get("force"));
        let job = app.start_job("privacy", json!({"state": "running", "progress": 0.0, "message": "démarrage"}));
        let app2 = app.clone();
        spawn(move || export::run_privacy(app2, job, items, force));
        return Ok(ok(json!({"ok": true})));
    }
    if p.len() == 3 && p[0] == "api" && p[1] == "export" && ["montage", "privacy", "follow"].contains(&p[2]) {
        let b = body_obj(body)?;
        if truthy(b.get("cancel")) {
            if let Some(job) = app.job(p[2]) {
                job.cancel();
            }
            return Ok(ok(json!({"ok": true})));
        }
    }
    if p.len() != 3 || !app.has(p[2]) {
        return Ok(Reply::Error(404));
    }
    let sid = p[2].to_string();
    match p[..2] {
        ["api", "offset"] => {
            let Value::Object(b) = body_json(body)? else { return Err(()) };
            let offset = b.get("offset_s").filter(|v| !v.is_null());
            let _g = app.lock.lock().unwrap();
            let mut ov: Map<String, Value> = read_json(&overrides_path()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
            match offset {
                None => {
                    ov.remove(&sid);
                }
                Some(v) => {
                    let f = float_of(Some(v), f64::NAN);
                    if f.is_nan() {
                        return Err(());
                    }
                    ov.insert(sid.clone(), json!(bike360_core::numeric::round_nd(f, 2)));
                }
            }
            write_json_indent(&overrides_path(), &Value::Object(ov.clone())).map_err(|_| ())?;
            let old = app.sess(&sid).ok_or(())?;
            let refs: Vec<(f64, f64)> = app.sessions.read().unwrap().iter()
                .filter(|(s2, s)| **s2 != sid && ["manuel", "corrélation"].contains(&s.result.offset_source.as_str()))
                .map(|(_, s)| (s.result.utc_t0, s.result.offset_s))
                .collect();
            let mut session = old.session.clone();
            let mut r = match analyze::analyze(&mut session, &ov, &refs, false) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Analyse de {sid} : {e:#}");
                    return Ok(Reply::Error(500));
                }
            };
            for (seg, info) in session.segments.iter_mut().zip(&r.segments) {
                seg.offset = info.offset;
                seg.duration = info.duration;
            }
            for k in ["folder", "parts"] {
                r.extra.insert(k.into(), old.result.extra.get(k).cloned().unwrap_or(if k == "parts" { json!(1) } else { Value::Null }));
            }
            let new = Arc::new(Sess { session, result: r });
            app.sessions.write().unwrap().insert(sid.clone(), new.clone());
            let mut out = result_json(&new);
            out.insert("selections".into(), Value::Array(app.get_selections(&sid).into_iter().map(Value::Object).collect()));
            Ok(ok(Value::Object(out)))
        }
        ["api", "export"] => {
            let b = body_obj(body)?;
            if truthy(b.get("cancel")) {
                if let Some(job) = app.job(&sid) {
                    job.cancel();
                }
                return Ok(ok(json!({"ok": true})));
            }
            let quality = b.get("quality").and_then(Value::as_str).unwrap_or("preview").to_string();
            if !["preview", "final"].contains(&quality.as_str()) {
                return Ok(Reply::Error(400));
            }
            let opts = export::export_opts(&b, &quality);
            if app.job_running(&sid) {
                return Ok(err(409, "export déjà en cours"));
            }
            let format = opts.get("format").cloned().unwrap_or(json!("standard"));
            let job = app.start_job(&sid, json!({"state": "running", "progress": 0.0, "quality": quality,
                                                 "message": "démarrage", "format": format}));
            let clips: Vec<(String, Map<String, Value>)> = app.get_selections(&sid).into_iter().map(|c| (sid.clone(), c)).collect();
            let app2 = app.clone();
            spawn(move || export::run_export(app2, job, sid, clips, quality, opts));
            Ok(ok(json!({"ok": true})))
        }
        ["api", "hyperlapse"] => {
            let b = body_obj(body)?;
            let duration = float_of(b.get("duration"), 180.0).clamp(30.0, 900.0);
            if truthy(b.get("preview")) {
                let s = app.sess(&sid).ok_or(())?;
                return Ok(ok(serde_json::to_value(hyperlapse::summary(&s.result, duration)).map_err(|_| ())?));
            }
            if app.job_running(&sid) {
                return Ok(err(409, "export déjà en cours"));
            }
            let height = int_of(b.get("height"), 1080);
            let mut view = Map::new();
            for (k, d) in [("yaw", 0.0), ("pitch", 0.0), ("roll", 0.0), ("fov", 100.0)] {
                view.insert(k.into(), json!(float_of(b.get(k), d)));
            }
            let hz = b.get("horizon").and_then(Value::as_str).filter(|h| ["auto", "fixe", "aucun"].contains(h)).unwrap_or("fixe");
            view.insert("horizon".into(), hz.into());
            let opts = HyperOpts {
                duration,
                height: if HEIGHTS.contains(&(height as u32)) { height as u32 } else { 1080 },
                view,
                format: b.get("format").and_then(Value::as_str).filter(|f| FORMATS.contains(f)).unwrap_or("standard").into(),
                crf: int_of(b.get("crf"), export::FINAL_CRF).clamp(14, 28),
            };
            let job = app.start_job(&sid, json!({"state": "running", "progress": 0.0, "quality": "hyperlapse",
                                                 "message": "démarrage"}));
            let app2 = app.clone();
            spawn(move || export::run_hyperlapse(app2, job, sid, opts));
            Ok(ok(json!({"ok": true})))
        }
        _ => Ok(Reply::Error(404)),
    }
}

/// Ajoute ou retire un dossier de vidéos, puis relance l'analyse en arrière-plan.
fn sources(app: &Arc<App>, b: &Value) -> Result<Reply, ()> {
    if app.scan.lock().unwrap().get("state").and_then(Value::as_str) == Some("running") {
        return Ok(err(409, "analyse déjà en cours"));
    }
    if truthy(b.get("rescan")) {   // nouvelle analyse à la demande (nouveaux fichiers, carte rebranchée)
        let app2 = app.clone();
        spawn(move || app2.rescan());
        return Ok(ok(json!({"ok": true})));
    }
    let mut extra: Vec<String> = read_json(&sources_path()).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    if truthy(b.get("add")) {
        let raw = pyjson::py_str(&b["add"]);
        let folder = match raw.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                PathBuf::from(format!("{}{rest}", std::env::var("HOME").unwrap_or_default()))
            }
            _ => PathBuf::from(&raw),
        };
        if !folder.is_dir() {
            return Ok(err(400, format!("dossier introuvable : {}", folder.display())));
        }
        let folder = std::fs::canonicalize(&folder).unwrap_or(folder).display().to_string();
        if insta360::scan(Path::new(&folder)).is_empty() {
            return Ok(err(400, "aucune vidéo Insta360 (.insv/.lrv) dans ce dossier"));
        }
        if !extra.contains(&folder) && folder != app.dcim {
            extra.push(folder);
        }
    } else if truthy(b.get("remove")) {
        let rm = b["remove"].clone();
        extra.retain(|f| Value::from(f.as_str()) != rm);
    }
    write_json_indent(&sources_path(), &json!(extra)).map_err(|_| ())?;
    let app2 = app.clone();
    spawn(move || app2.rescan());
    Ok(ok(json!({"ok": true})))
}

/// Reçoit une musique (corps brut) : POST /api/music?name=fichier.mp3.
fn music_upload(full: &str, headers: &HeaderMap, body: &Bytes) -> Reply {
    let qs = raw_query(full);
    let raw = unquote(qs.get("name").and_then(Value::as_str).unwrap_or(""), false);
    let name = Path::new(&raw).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let n: usize = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
    let ext = Path::new(&name).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !MUSIC_EXT.contains(&ext.as_str()) || name.trim_matches('.').is_empty() {
        return err(400, "format non pris en charge (mp3, m4a, aac, wav, ogg, opus, flac)");
    }
    if !(0 < n && n <= MUSIC_MAX_BYTES) {
        return err(400, "fichier vide ou trop gros (60 Mo max)");
    }
    if std::fs::create_dir_all(music_dir()).and_then(|_| std::fs::write(music_dir().join(&name), body)).is_err() {
        return Reply::Error(500);
    }
    ok(json!({"ok": true, "name": name}))
}

/// Traces GPX déposées : [{name, points, from, to}] (dates UTC, absentes d'un fichier sans point horodaté).
fn gps_files() -> Vec<Value> {
    let iso = |t: f64| chrono::DateTime::from_timestamp(t as i64, 0).map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    gpx::files().iter().map(|f| {
        let (points, span) = gpx::summary(f);
        json!({"name": f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "points": points,
               "from": span.and_then(|s| iso(s.0)), "to": span.and_then(|s| iso(s.1))})
    }).collect()
}

/// Reçoit une trace GPS (corps brut) : POST /api/gps?name=trace.gpx, ou la retire : POST /api/gps?remove=trace.gpx.
/// Les sessions du jour sont ensuite analysées de nouveau avec cette source.
fn gps_upload(app: &Arc<App>, full: &str, headers: &HeaderMap, body: &Bytes) -> Reply {
    let qs = raw_query(full);
    let file_name = |key: &str| {
        let raw = unquote(qs.get(key).and_then(Value::as_str).unwrap_or(""), false);
        Path::new(&raw).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
    };
    let is_gpx = |name: &str| Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case("gpx")) && !name.starts_with('.');
    if app.scan.lock().unwrap().get("state").and_then(Value::as_str) == Some("running") {
        return err(409, "analyse déjà en cours");
    }
    let removed = file_name("remove");
    let name = file_name("name");
    if !removed.is_empty() {
        if !is_gpx(&removed) || std::fs::remove_file(gpx::dir().join(&removed)).is_err() {
            return err(404, "trace introuvable");
        }
    } else {
        let n: usize = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok()).unwrap_or(0);
        if !is_gpx(&name) {
            return err(400, "format non pris en charge (fichier .gpx attendu)");
        }
        if !(0 < n && n <= GPX_MAX_BYTES) {
            return err(400, "fichier vide ou trop gros (60 Mo max)");
        }
        if gpx::parse(&String::from_utf8_lossy(body)).len() < 2 {
            return err(400, "aucun point horodaté dans ce fichier GPX");
        }
        if std::fs::create_dir_all(gpx::dir()).and_then(|_| std::fs::write(gpx::dir().join(&name), body)).is_err() {
            return Reply::Error(500);
        }
    }
    let app2 = app.clone();
    spawn(move || app2.rescan());
    ok(json!({"ok": true, "files": gps_files()}))
}

/// Montage automatique : remplace les clips auto des sessions par un nouveau plan.
fn automontage_route(app: &Arc<App>, b: &Value) -> Result<Reply, ()> {
    let src: Vec<Value> = match b.get("sids") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        _ => app.get_project()["sessions"].as_array().cloned().unwrap_or_default(),
    };
    let sids: Vec<String> = src.iter().filter_map(Value::as_str).filter(|s| app.has(s)).map(String::from).collect();
    let target = float_of(b.get("duration"), 120.0).clamp(20.0, 900.0);
    let _g = app.lock.lock().unwrap();
    let mut removed = 0;
    let mut existing: std::collections::HashMap<String, Vec<Map<String, Value>>> = Default::default();
    for sid in &sids {
        // les clips auto précédents sont remplacés ; les tiens restent
        let Ok(clips) = app.try_selections(sid) else { return Ok(err(503, "clips momentanément illisibles, réessaie")) };
        let keep: Vec<Map<String, Value>> = clips.iter().filter(|c| !truthy(c.get("auto"))).cloned().collect();
        if keep.len() != clips.len() {
            removed += clips.len() - keep.len();
            app.write_selections(sid, &keep).map_err(|_| ())?;
        }
        existing.insert(sid.clone(), keep);
    }
    if truthy(b.get("clear")) {
        return Ok(ok(json!({"removed": removed, "added": 0})));
    }
    let style = app.get_project()["style"].clone();
    let transition = if style["transition"] != json!("aucune") { style["duration"].as_f64().unwrap_or(0.6) } else { 0.0 };
    let results: std::collections::BTreeMap<String, analyze::Analysis> =
        sids.iter().map(|sid| (sid.clone(), app.sess(sid).unwrap().result.clone())).collect();
    let parsed: std::collections::HashMap<String, Vec<geometry::Clip>> = existing.iter()
        .map(|(sid, cs)| (sid.clone(), cs.iter().filter_map(|c| export::to_clip(c).ok()).collect()))
        .collect();
    let plan = automontage::plan(&results, target, &parsed, transition);
    let mut added = vec![];
    for (sid, clips) in &plan {
        let mut all = existing[sid].clone();
        let front = app.sess(sid).map_or(0.0, |s| position::front_yaw(&session_position(&s)));
        for c in clips {
            // même ordre de clés que la version Python
            let mut m = Map::new();
            m.insert("id".into(), json!(c.id));
            m.insert("start".into(), json!(c.start));
            m.insert("end".into(), json!(c.end));
            m.insert("yaw".into(), json!(c.yaw + front));
            m.insert("pitch".into(), json!(c.pitch));
            m.insert("fov".into(), json!(c.fov));
            m.insert("roll".into(), json!(c.roll));
            m.insert("horizon".into(), json!(c.horizon));
            m.insert("auto".into(), json!(true));
            all.push(m);
            added.push(c.end - c.start);
        }
        all.sort_by(|a, b| float_of(a.get("start"), 0.0).total_cmp(&float_of(b.get("start"), 0.0)));
        app.write_selections(sid, &all).map_err(|_| ())?;
    }
    Ok(ok(json!({"removed": removed, "added": added.len(),
                 "seconds": bike360_core::numeric::round_nd(added.iter().sum(), 1)})))
}
