//! Serveur local de l'outil de tri : UI, streaming des proxys .lrv, sélections, export.
//!
//! Usage : bike360-server [DCIM] [--port 8360] [--host 127.0.0.1]
//! Portage Rust de server.py : mêmes routes, mêmes JSON, mêmes fichiers de data/.

mod app;
mod audio;
mod auth;
mod export;
mod privacy;
mod pyjson;
mod routes;
mod sources;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::Router;
use clap::Parser;

use crate::app::{App, HORIZON_WORKERS};

/// Carte SD de la caméra telle que la monte le bureau Linux (utilisateur courant).
fn default_dcim() -> String {
    format!("/run/media/{}/Insta360 X5/DCIM", std::env::var("USER").unwrap_or_default())
}

#[derive(Parser)]
#[command(about = "Serveur local de l'outil de tri et de montage Insta360 X5")]
struct Args {
    /// Dossier des vidéos (carte SD)
    #[arg(default_value_t = default_dcim())]
    dcim: String,
    #[arg(long, default_value_t = 8360)]
    port: u16,
    /// adresse d'écoute (ex. l'IP Wi-Fi pour un téléphone) ; hors 127.0.0.1, définir BIKE360_PASSWORD
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    if auth::init() {
        println!("Authentification activée (BIKE360_PASSWORD).");
    } else if a.host != "127.0.0.1" && a.host != "localhost" {
        eprintln!("⚠ Serveur ouvert sur {} sans mot de passe : définir BIKE360_PASSWORD.", a.host);
    }
    let app = App::new(a.dcim.clone());
    app.nvenc_available();
    println!("Analyse des sessions…");
    {
        let app = app.clone();
        tokio::task::spawn_blocking(move || app.load_sessions()).await??;
    }
    {
        let app = app.clone();
        std::thread::spawn(move || sources::watch(app));
    }
    for _ in 0..HORIZON_WORKERS {
        let app = app.clone();
        std::thread::spawn(move || app.horizon_worker());
    }
    let mut by_len: Vec<(String, usize)> =
        app.sessions.read().unwrap().iter().map(|(k, s)| (k.clone(), s.result.duration)).collect();
    by_len.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
    for (sid, _) in by_len {
        app.request_horizon(&sid, false);
    }
    let router = Router::new()
        .fallback(routes::dispatch)
        .layer(DefaultBodyLimit::max(70 * 1024 * 1024))
        .with_state(Arc::clone(&app));
    let listener = tokio::net::TcpListener::bind(format!("{}:{}", a.host, a.port)).await?;
    println!("→ http://{}:{}/", a.host, a.port);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
