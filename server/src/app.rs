//! État du serveur : sessions analysées, tâches de fond, file de calcul de l'horizon, et
//! fichiers de data/ (sélections, projet, réglages, dossiers sources, décalages).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

use anyhow::Result;
use bike360_core::analyze::{self, Analysis};
use bike360_core::horizon::{self, HorizonData};
use bike360_core::insta360::{self, Session};
use bike360_core::{automontage, finishing, paths, telemetry};
use serde_json::{json, Map, Value};

use crate::pyjson;

pub const PROJECT_MIN_S: usize = 60; // sans projet enregistré : sessions d'au moins une minute
pub const HORIZON_WORKERS: usize = 3; // calculs d'horizon simultanés
pub const MUSIC_EXT: [&str; 7] = ["mp3", "m4a", "aac", "wav", "ogg", "opus", "flac"];

// ---------------------------------------------------------------- emplacements

pub fn selections_dir() -> PathBuf {
    paths::data().join("selections")
}
pub fn exports_dir() -> PathBuf {
    paths::root().join("exports")
}
pub fn settings_path() -> PathBuf {
    paths::data().join("settings.json")
}
pub fn sources_path() -> PathBuf {
    paths::data().join("sources.json")
}
pub fn project_path() -> PathBuf {
    paths::data().join("project.json")
}
pub fn overrides_path() -> PathBuf {
    paths::data().join("overrides.json")
}
pub fn thumbs_dir() -> PathBuf {
    paths::cache().join("thumbs")
}
pub fn music_dir() -> PathBuf {
    paths::data().join("music")
}
/// Versions précédentes gardées par session (historique des clips).
const HISTORY_KEEP: usize = 200;

pub fn selections_path(sid: &str) -> PathBuf {
    selections_dir().join(format!("{sid}.json"))
}

include!(concat!(env!("OUT_DIR"), "/ui_files.rs"));

/// Fichier de l'interface web (`rel` : chemin sous ui/) : <racine>/ui sur disque en priorité,
/// sinon la copie embarquée à la compilation. None si introuvable ou chemin suspect.
pub enum UiFile {
    Disk(PathBuf),
    Embedded(&'static [u8]),
}

pub fn ui_file(rel: &str) -> Option<UiFile> {
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    let p = paths::root().join("ui").join(rel);
    if p.is_file() {
        return Some(UiFile::Disk(p));
    }
    UI_FILES.iter().find(|(name, _)| *name == rel).map(|(_, data)| UiFile::Embedded(data))
}

/// Moteur GPU : à côté de ce binaire, sinon <racine>/target/release/bike360-render.
pub fn render_bin() -> PathBuf {
    if let Some(p) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("bike360-render"))) {
        if p.exists() {
            return p;
        }
    }
    paths::root().join("target/release/bike360-render")
}

/// Écrit un fichier JSON au format de Python (`json.dumps(v, indent=1)`).
pub fn write_json_indent(path: &Path, v: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(path, pyjson::dumps_indent(v, 1).as_bytes())
}

/// Écriture atomique (fichier temporaire puis renommage) : une lecture simultanée voit
/// l'ancien ou le nouveau contenu, jamais un fichier vide ou tronqué.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}-{:?}", std::process::id(), std::thread::current().id())
        .replace(['(', ')', ' '], ""));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| { let _ = std::fs::remove_file(&tmp); })?;
    Ok(())
}

pub fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}

// ---------------------------------------------------------------- tâches

/// Tâche de fond (export, analyse de confidentialité, suivi) : état JSON exposé à l'interface,
/// processus en cours (pour l'annulation) et drapeau d'annulation.
#[derive(Default)]
pub struct Job {
    data: Mutex<Map<String, Value>>,
    pids: Mutex<Vec<u32>>,
    cancelled: AtomicBool,
}

impl Job {
    pub fn new(init: Value) -> Arc<Job> {
        let job = Job::default();
        if let Value::Object(m) = init {
            *job.data.lock().unwrap() = m;
        }
        Arc::new(job)
    }
    pub fn set(&self, k: &str, v: impl Into<Value>) {
        self.data.lock().unwrap().insert(k.into(), v.into());
    }
    pub fn update(&self, v: Value) {
        if let Value::Object(m) = v {
            let mut d = self.data.lock().unwrap();
            for (k, v) in m {
                d.insert(k, v);
            }
        }
    }
    pub fn get(&self, k: &str) -> Option<Value> {
        self.data.lock().unwrap().get(k).cloned()
    }
    pub fn snapshot(&self) -> Value {
        Value::Object(self.data.lock().unwrap().clone())
    }
    pub fn running(&self) -> bool {
        self.get("state").and_then(|v| v.as_str().map(|s| s == "running")).unwrap_or(false)
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    pub fn add_pid(&self, pid: u32) {
        self.pids.lock().unwrap().push(pid);
    }
    pub fn remove_pid(&self, pid: u32) {
        self.pids.lock().unwrap().retain(|p| *p != pid);
    }
    /// Annulation : drapeau (visible dans l'état) et SIGTERM aux processus en cours.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.set("cancelled", true);
        for pid in self.pids.lock().unwrap().iter() {
            unsafe {
                libc::kill(*pid as i32, libc::SIGTERM);
            }
        }
    }
    /// Fin d'une tâche : état d'erreur (message) si elle a échoué.
    pub fn finish(&self, r: Result<()>) {
        if let Err(e) = r {
            eprintln!("Erreur : {e:#}");
            self.update(json!({"state": "error", "message": e.to_string()}));
        }
        self.pids.lock().unwrap().clear();
    }
}

// ---------------------------------------------------------------- horizon

#[derive(Clone)]
pub struct HorizonEntry {
    pub status: String,
    pub progress: f64,
    pub message: Option<String>,
    pub data: Option<Arc<HorizonData>>,
}

impl HorizonEntry {
    /// État JSON (sans les données), comme la version Python.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("status".into(), self.status.clone().into());
        m.insert("progress".into(), self.progress.into());
        if let Some(msg) = &self.message {
            m.insert("message".into(), msg.clone().into());
        }
        Value::Object(m)
    }
}

#[derive(Default)]
pub struct HorizonQueue {
    pub entries: HashMap<String, HorizonEntry>,
    pub queue: VecDeque<String>,
}

// ---------------------------------------------------------------- application

/// Session (bloc continu) et son analyse (avec `folder` et `parts` dans `extra`).
pub struct Sess {
    pub session: Session,
    pub result: Analysis,
}

pub struct App {
    pub dcim: String,
    pub sessions: RwLock<BTreeMap<String, Arc<Sess>>>,
    pub jobs: Mutex<HashMap<String, Arc<Job>>>,
    pub nvenc: OnceLock<bool>,
    pub horizon: Mutex<HorizonQueue>,
    pub horizon_cv: Condvar,
    pub scan: Mutex<Value>,
    /// Équivalent du verrou global Python (analyse des dossiers, décalage, montage auto, suivi).
    pub lock: Mutex<()>,
    durations: Mutex<HashMap<(PathBuf, u64), f64>>,
}

impl App {
    pub fn new(dcim: String) -> Arc<App> {
        Arc::new(App {
            dcim,
            sessions: RwLock::new(BTreeMap::new()),
            jobs: Mutex::new(HashMap::new()),
            nvenc: OnceLock::new(),
            horizon: Mutex::new(HorizonQueue::default()),
            horizon_cv: Condvar::new(),
            scan: Mutex::new(json!({"state": "idle"})),
            lock: Mutex::new(()),
            durations: Mutex::new(HashMap::new()),
        })
    }

    pub fn sess(&self, sid: &str) -> Option<Arc<Sess>> {
        self.sessions.read().unwrap().get(sid).cloned()
    }
    pub fn has(&self, sid: &str) -> bool {
        self.sessions.read().unwrap().contains_key(sid)
    }
    pub fn job(&self, key: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(key).cloned()
    }
    pub fn job_running(&self, key: &str) -> bool {
        self.job(key).is_some_and(|j| j.running())
    }
    pub fn start_job(&self, key: &str, init: Value) -> Arc<Job> {
        let job = Job::new(init);
        self.jobs.lock().unwrap().insert(key.into(), job.clone());
        job
    }

    /// Teste une fois si l'encodeur GPU NVIDIA fonctionne (pilote assez récent).
    pub fn nvenc_available(&self) -> bool {
        *self.nvenc.get_or_init(|| {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=s=256x144:d=0.2", "-c:v", "h264_nvenc", "-f", "null", "-"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            println!("Encodeur GPU NVENC : {}", if ok { "disponible" } else { "indisponible (x264 utilisé)" });
            ok
        })
    }

    /// Moteur Rust/CUDA compilé (render/) et NVENC utilisable.
    pub fn gpu_engine_available(&self) -> bool {
        render_bin().exists() && self.nvenc_available()
    }

    // ------------------------------------------------------------ dossiers et sessions

    /// Dossier de la ligne de commande (carte SD) puis dossiers ajoutés depuis l'interface.
    pub fn source_folders(&self) -> Vec<String> {
        let extra: Vec<String> = read_json(&sources_path())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let mut out: Vec<String> = vec![];
        for f in std::iter::once(self.dcim.clone()).chain(extra) {
            if !out.contains(&f) {
                out.push(f);
            }
        }
        out
    }

    /// Durée (s) d'une session d'origine : somme des .lrv (ffprobe, mémorisée).
    fn session_duration(&self, s: &Session) -> f64 {
        let mut total = 0.0;
        for seg in &s.segments {
            let Some(lrv) = &seg.lrv else { continue };
            let size = std::fs::metadata(lrv).map(|m| m.len()).unwrap_or(0);
            let key = (lrv.clone(), size);
            let cached = self.durations.lock().unwrap().get(&key).copied();
            total += match cached {
                Some(d) => d,
                None => {
                    let d = analyze::file_duration(lrv);
                    self.durations.lock().unwrap().insert(key, d);
                    d
                }
            };
        }
        total
    }

    /// Analyse les sessions de tous les dossiers présents (analyses en cache réutilisées).
    ///
    /// Une session présente dans deux dossiers n'est prise qu'une fois (la première trouvée) ;
    /// les fichiers qui se suivent sans interruption forment un seul bloc continu.
    pub fn load_sessions(&self) -> Result<()> {
        self.load_sessions_with(&mut |_| {})
    }

    /// Comme [`load_sessions`], en signalant l'avancement : `report({phase, message, done, total, current})`.
    pub fn load_sessions_with(&self, report: &mut dyn FnMut(Value)) -> Result<()> {
        let mut by_id: Vec<Session> = vec![];
        let mut origin: HashMap<String, String> = HashMap::new();
        let folders = self.source_folders();
        for (i, folder) in folders.iter().enumerate() {
            report(json!({"phase": "listing", "message": format!("recherche des vidéos ({}/{}) : {}", i + 1, folders.len(), folder),
                          "done": i, "total": folders.len(), "current": folder}));
            if !Path::new(folder).is_dir() {
                continue;
            }
            for s in insta360::scan(Path::new(folder)) {
                if !origin.contains_key(&s.id) {
                    origin.insert(s.id.clone(), folder.clone());
                    by_id.push(s);
                }
            }
        }
        let mut durations: HashMap<String, f64> = HashMap::new();
        for (i, s) in by_id.iter().enumerate() {
            report(json!({"phase": "durations", "message": format!("lecture des durées ({}/{})", i + 1, by_id.len()),
                          "done": i, "total": by_id.len(), "current": s.id}));
            durations.insert(s.id.clone(), self.session_duration(s));
        }
        let blocks = insta360::merge_continuous(by_id, |s| durations[&s.id]);
        for b in &blocks {
            if !b.parts.is_empty() {
                let mut members = vec![];
                let mut off = 0.0;
                for sid in &b.parts {
                    members.push((sid.clone(), off));
                    off += durations[sid];
                }
                migrate_block_selections(&b.id, &members)?;
            }
        }
        let results = analyze::analyze_sessions_progress(blocks, false, &mut |done, total, sid| {
            if !sid.is_empty() {
                report(json!({"phase": "analyzing", "message": format!("analyse des sessions ({}/{}) : {sid}", done + 1, total),
                              "done": done, "total": total, "current": sid}));
            }
        })?;
        let mut map = BTreeMap::new();
        for (s, mut r) in results {
            // Bloc : dossier de la première session (le bloc porte son identifiant).
            let folder = origin.get(&s.id).cloned();
            r.extra.insert("folder".into(), folder.map(Value::from).unwrap_or(Value::Null));
            r.extra.insert("parts".into(), (s.parts.len().max(1)).into());
            map.insert(s.id.clone(), Arc::new(Sess { session: s, result: r }));
        }
        *self.sessions.write().unwrap() = map;
        Ok(())
    }

    /// Nouvelle analyse des dossiers (après ajout/retrait d'un dossier), en tâche de fond.
    pub fn rescan(self: &Arc<Self>) {
        let started = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let running = |patch: Value| {
            let mut v = json!({"state": "running", "started": started, "phase": "listing", "message": "analyse des dossiers…"});
            if let (Some(a), Value::Object(b)) = (v.as_object_mut(), patch) {
                a.extend(b);
            }
            v
        };
        *self.scan.lock().unwrap() = running(json!({}));
        let before: HashSet<String> = self.sessions.read().unwrap().keys().cloned().collect();
        let r = {
            let _g = self.lock.lock().unwrap();
            self.load_sessions_with(&mut |patch| *self.scan.lock().unwrap() = running(patch))
        };
        match r {
            Ok(()) => {
                let mut new: Vec<String> =
                    self.sessions.read().unwrap().keys().filter(|k| !before.contains(*k)).cloned().collect();
                new.sort();
                for sid in &new {
                    self.request_horizon(sid, false);
                }
                let msg = if new.is_empty() { "aucune nouvelle session".to_string() } else { format!("{} nouvelle(s) session(s)", new.len()) };
                *self.scan.lock().unwrap() = json!({"state": "done", "message": msg, "new": new, "started": started,
                                                    "finished": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())});
            }
            Err(e) => {
                eprintln!("Erreur d'analyse : {e:#}");
                *self.scan.lock().unwrap() = json!({"state": "error", "message": e.to_string()});
            }
        }
    }

    // ------------------------------------------------------------ horizon

    /// Met une session dans la file de calcul de l'horizon (en tête si urgent).
    pub fn request_horizon(&self, sid: &str, urgent: bool) -> HorizonEntry {
        let mut h = self.horizon.lock().unwrap();
        let e = h.entries.entry(sid.into())
            .or_insert(HorizonEntry { status: "queued".into(), progress: 0.0, message: None, data: None })
            .clone();
        if e.status == "done" || e.status == "running" {
            return e;
        }
        h.queue.retain(|x| x != sid);
        if urgent {
            h.queue.push_front(sid.into());
        } else {
            h.queue.push_back(sid.into());
        }
        let e = h.entries.get_mut(sid).unwrap();
        e.status = "queued".into();
        let out = e.clone();
        self.horizon_cv.notify_one();
        out
    }

    pub fn horizon_worker(self: Arc<Self>) {
        loop {
            let sid = {
                let mut h = self.horizon.lock().unwrap();
                while h.queue.is_empty() {
                    h = self.horizon_cv.wait(h).unwrap();
                }
                let sid = h.queue.pop_front().unwrap();
                h.entries.get_mut(&sid).unwrap().status = "running".into();
                sid
            };
            let r = match self.sess(&sid) {
                Some(s) => {
                    let progress = |f: f64| {
                        if let Some(e) = self.horizon.lock().unwrap().entries.get_mut(&sid) {
                            e.progress = f;
                        }
                    };
                    horizon::compute(&s.session, &s.result, &paths::cache(), Some(&progress))
                }
                None => Err(anyhow::anyhow!("session inconnue : {sid}")),
            };
            let mut h = self.horizon.lock().unwrap();
            let e = h.entries.get_mut(&sid).unwrap();
            match r {
                Ok(data) => {
                    e.status = "done".into();
                    e.progress = 1.0;
                    e.data = Some(Arc::new(data));
                }
                Err(err) => {
                    eprintln!("Horizon de {sid} : {err:#}");
                    e.status = "error".into();
                    e.message = Some(err.to_string());
                }
            }
            self.horizon_cv.notify_all();
        }
    }

    /// Horizon complet d'une session s'il est prêt.
    pub fn horizon_done(&self, sid: &str) -> Option<Arc<HorizonData>> {
        let h = self.horizon.lock().unwrap();
        h.entries.get(sid).filter(|e| e.status == "done").and_then(|e| e.data.clone())
    }

    // ------------------------------------------------------------ réglages, clips, projet

    pub fn get_settings(&self) -> Value {
        let cfg = read_json(&settings_path()).unwrap_or(json!({}));
        let mut tel = telemetry_defaults();
        if let Some(Value::Object(t)) = cfg.get("telemetry") {
            for (k, v) in t {
                tel.insert(k.clone(), v.clone());
            }
        }
        let mut privacy = Map::new();
        privacy.insert("enabled".into(), false.into());
        if let Some(Value::Object(p)) = cfg.get("privacy") {
            for (k, v) in p {
                privacy.insert(k.clone(), v.clone());
            }
        }
        json!({"masks": cfg.get("masks").cloned().unwrap_or(json!([])), "telemetry": tel, "privacy": privacy})
    }

    /// Clips d'une session ; ceux d'avant les identifiants en reçoivent un (ordre du montage).
    pub fn get_selections(&self, sid: &str) -> Vec<Map<String, Value>> {
        self.try_selections(sid).unwrap_or_else(|e| {
            eprintln!("  clips de {sid} illisibles : {e:#}");
            vec![]
        })
    }

    /// Clips d'une session ; erreur si le fichier existe mais ne se lit pas (jamais une liste
    /// vide à la place : l'interface la réenregistrerait et effacerait les clips).
    pub fn try_selections(&self, sid: &str) -> Result<Vec<Map<String, Value>>> {
        let p = selections_path(sid);
        if !p.exists() {
            return Ok(vec![]);
        }
        let mut last = None;
        let mut clips: Vec<Map<String, Value>> = loop {
            match std::fs::read_to_string(&p).map_err(anyhow::Error::from)
                .and_then(|t| Ok(serde_json::from_str::<Vec<Map<String, Value>>>(&t)?)) {
                Ok(c) => break c,
                Err(e) if last.is_none() => {   // une seconde chance (écriture d'un ancien serveur)
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e.context(format!("lecture de {p:?}"))),
            }
        };
        if clips.iter().any(|c| !c.contains_key("id")) {
            for c in clips.iter_mut() {
                c.entry("id").or_insert_with(|| automontage::new_id().into());
            }
            let _ = write_json_indent(&p, &Value::from(clips.iter().cloned().map(Value::Object).collect::<Vec<_>>()));
        }
        Ok(clips)
    }

    /// Enregistre les clips d'une session ; la version précédente est d'abord gardée dans
    /// selections/historique/<session>/ (les HISTORY_KEEP dernières), pour pouvoir revenir en arrière.
    pub fn write_selections(&self, sid: &str, clips: &[Map<String, Value>]) -> Result<()> {
        let path = selections_path(sid);
        if let Ok(prev) = std::fs::read(&path) {
            let dir = selections_dir().join("historique").join(sid);
            std::fs::create_dir_all(&dir)?;
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
            write_atomic(&dir.join(format!("{stamp}.json")), &prev)?;
            let mut old: Vec<PathBuf> = std::fs::read_dir(&dir)?.flatten().map(|e| e.path()).collect();
            old.sort();
            for f in old.iter().rev().skip(HISTORY_KEEP) {
                let _ = std::fs::remove_file(f);
            }
        }
        write_json_indent(&path, &Value::Array(clips.iter().cloned().map(Value::Object).collect()))
    }

    /// Projet : sessions retenues, ordre libre du montage [[sid, id du clip]], clips exclus.
    pub fn get_project(&self) -> Value {
        let sessions = self.sessions.read().unwrap();
        let proj = read_json(&project_path()).unwrap_or_else(|| {
            let sids: Vec<&String> = sessions.iter()
                .filter(|(sid, s)| s.result.duration >= PROJECT_MIN_S || !self.get_selections(sid).is_empty())
                .map(|(sid, _)| sid)
                .collect();
            json!({"sessions": sids})
        });
        let list = |k: &str| proj.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        let kept: Vec<Value> = list("sessions").into_iter()
            .filter(|x| x.as_str().is_some_and(|s| sessions.contains_key(s)))
            .collect();
        json!({"sessions": kept, "order": list("order"), "excluded": list("excluded"),
               "style": clean_style(proj.get("style"))})
    }

    /// Clips du montage dans l'ordre : ordre enregistré, puis les nouveaux clips chronologiquement.
    /// (sid, clip, exclu)
    pub fn montage_items_all(&self, proj: Option<&Value>) -> Vec<(String, Map<String, Value>, bool)> {
        let owned;
        let proj = match proj {
            Some(p) => p,
            None => {
                owned = self.get_project();
                &owned
            }
        };
        let pair = |x: &Value| -> Option<(String, String)> {
            let a = x.as_array()?;
            (a.len() == 2).then(|| Some((a[0].as_str()?.to_string(), a[1].as_str()?.to_string())))?
        };
        let mut keys: Vec<(String, String)> = vec![];
        let mut clips: HashMap<(String, String), Map<String, Value>> = HashMap::new();
        for sid in proj["sessions"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            for c in self.get_selections(sid) {
                let id = c.get("id").map(|v| pyjson::py_str(v)).unwrap_or_default();
                let k = (sid.to_string(), id);
                if !clips.contains_key(&k) {
                    keys.push(k.clone());
                }
                clips.insert(k, c);
            }
        }
        let mut order: Vec<(String, String)> = vec![];
        for k in proj["order"].as_array().into_iter().flatten().filter_map(pair) {
            if clips.contains_key(&k) {
                order.push(k);
            }
        }
        let in_order: HashSet<&(String, String)> = order.iter().collect();
        let sessions = self.sessions.read().unwrap();
        let utc = |k: &(String, String)| {
            sessions.get(&k.0).map_or(0.0, |s| s.result.utc_t0) + clips[k].get("start").and_then(Value::as_f64).unwrap_or(0.0)
        };
        let mut rest: Vec<(String, String)> = keys.iter().filter(|k| !in_order.contains(k)).cloned().collect();
        rest.sort_by(|a, b| utc(a).total_cmp(&utc(b)));
        let excluded: HashSet<(String, String)> = proj["excluded"].as_array().into_iter().flatten().filter_map(pair).collect();
        order.iter().chain(&rest)
            .map(|k| (k.0.clone(), clips[k].clone(), excluded.contains(k)))
            .collect()
    }

    pub fn montage_items(&self) -> Vec<(String, Map<String, Value>)> {
        self.montage_items_all(None).into_iter().filter(|(_, _, ex)| !ex).map(|(s, c, _)| (s, c)).collect()
    }

    pub fn music_files(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(music_dir()).into_iter().flatten().flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let ext = Path::new(&name).extension()?.to_string_lossy().to_lowercase();
                MUSIC_EXT.contains(&ext.as_str()).then_some(name)
            })
            .collect();
        out.sort();
        out
    }
}

/// Réglages de télémétrie par défaut (telemetry::DEFAULTS, mêmes clés que les réglages).
pub fn telemetry_defaults() -> Map<String, Value> {
    match serde_json::to_value(telemetry::DEFAULTS) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    }
}

/// Style du montage validé ; un style illisible (valeur non numérique) revient aux défauts.
pub fn clean_style(style: Option<&Value>) -> Value {
    finishing::clean(style).unwrap_or_else(|_| finishing::defaults())
}

/// Reporte sur le bloc les clips posés sur ses morceaux avant la fusion (décalés dans le temps).
///
/// `members` : [(session d'origine, début dans le bloc en s)]. Le fichier du morceau est
/// renommé en .fusionné.json pour ne pas être repris deux fois.
fn migrate_block_selections(block_id: &str, members: &[(String, f64)]) -> Result<()> {
    let mut moved: Vec<Map<String, Value>> = vec![];
    for (sid, offset) in members {
        let path = selections_path(sid);
        if sid == block_id || !path.exists() {
            continue;
        }
        let clips: Vec<Map<String, Value>> = read_json(&path).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        for mut c in clips {
            for k in ["start", "end"] {
                let v = c.get(k).and_then(Value::as_f64).unwrap_or(0.0);
                c.insert(k.into(), bike360_core::numeric::round_nd(v + offset, 2).into());
            }
            moved.push(c);
        }
        std::fs::rename(&path, path.with_extension("fusionné.json"))?;
    }
    if !moved.is_empty() {
        let p = selections_path(block_id);
        let mut clips: Vec<Map<String, Value>> = read_json(&p).and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        for c in clips.iter_mut() {
            c.entry("id").or_insert_with(|| automontage::new_id().into());
        }
        let n = moved.len();
        clips.extend(moved);
        let start = |c: &Map<String, Value>| c.get("start").and_then(Value::as_f64).unwrap_or(0.0);
        clips.sort_by(|a, b| start(a).total_cmp(&start(b)));
        write_json_indent(&p, &Value::Array(clips.into_iter().map(Value::Object).collect()))?;
        println!("  {n} clip(s) reporté(s) sur le bloc {block_id}");
    }
    Ok(())
}
