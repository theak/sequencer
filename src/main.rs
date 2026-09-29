//! AK-16 Sequencer: an 808-style drum machine + synth sequencer.
//!
//! The whole frontend is one HTML file baked into the binary. The server adds two
//! things a static page can't do on its own: saved beats (JSON files under `DATA_DIR`)
//! and AI beat generation (a streaming proxy to OpenRouter, so the API key stays
//! server side).

mod ai;
mod handlers;
mod store;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post, put},
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 42716;
pub const DEFAULT_OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";

/// OpenRouter's `~…-latest` aliases always resolve to the newest model in each family,
/// so the defaults never go stale. Override with `OPENROUTER_MODELS`.
pub const DEFAULT_MODELS: &str = "~anthropic/claude-sonnet-latest=Claude Sonnet,\
~anthropic/claude-haiku-latest=Claude Haiku,\
~openai/gpt-luna-latest=GPT Luna,\
~google/gemini-flash-latest=Gemini Flash";

/// Largest request body we accept (a 4-page beat with several synths is ~10 kB; a
/// generation prompt with "build on current beat" is ~20 kB).
pub const BODY_LIMIT: usize = 256 * 1024;
/// `/api/generate` can also carry the beat's spectrogram as a base64 image.
pub const GENERATE_BODY_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub id: String,
    pub label: String,
}

/// Runtime configuration, read once at boot.
///
/// The server boots and serves the app even with no OpenRouter key; generation is then
/// reported as unavailable via `/api/config` and `/api/generate` returns 503.
pub struct Config {
    pub data_dir: PathBuf,
    pub openrouter_key: Option<String>,
    pub openrouter_base: String,
    pub models: Vec<Model>,
}

/// `slug=Label,slug2=Label2` → models. A missing label falls back to the slug.
pub fn parse_models(s: &str) -> Vec<Model> {
    s.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| match e.split_once('=') {
            Some((id, label)) if !label.trim().is_empty() => Model {
                id: id.trim().to_string(),
                label: label.trim().to_string(),
            },
            Some((id, _)) => Model {
                id: id.trim().to_string(),
                label: id.trim().to_string(),
            },
            None => Model {
                id: e.to_string(),
                label: e.to_string(),
            },
        })
        .filter(|m| !m.id.is_empty())
        .collect()
}

/// Read env into `Config`. An empty string counts as unset.
pub fn load_config() -> Config {
    fn var(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|s| !s.is_empty())
    }
    Config {
        data_dir: PathBuf::from(var("DATA_DIR").unwrap_or_else(|| "/data".into())),
        openrouter_key: var("OPENROUTER_API_KEY"),
        openrouter_base: var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|| DEFAULT_OPENROUTER_BASE.into())
            .trim_end_matches('/')
            .to_string(),
        models: parse_models(&var("OPENROUTER_MODELS").unwrap_or_else(|| DEFAULT_MODELS.into())),
    }
}

/// Shared, cheaply-cloneable application state. `reqwest::Client` is internally `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<store::Store>,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(cfg: Config) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        AppState {
            store: Arc::new(store::Store::new(cfg.data_dir.clone())),
            cfg: Arc::new(cfg),
            client,
        }
    }
}

/// Build the app router. Kept public so tests can drive it via `oneshot`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/healthz", get(handlers::healthz))
        .route("/api/config", get(handlers::config))
        .route(
            "/api/beats",
            get(handlers::list_beats).post(handlers::create_beat),
        )
        .route(
            "/api/beats/{id}",
            put(handlers::update_beat).delete(handlers::delete_beat),
        )
        .route(
            "/api/generate",
            post(ai::generate).layer(DefaultBodyLimit::max(GENERATE_BODY_LIMIT)),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(state)
}

fn port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

/// Resolve on Ctrl-C or SIGTERM (what `docker stop` and Cloud Run send).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
}

#[tokio::main]
async fn main() {
    // `healthcheck` subcommand: TCP-connect to our own port, exit 0/1 (for scratch, no curl).
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        let ok = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(("127.0.0.1", port())),
        )
        .await
        .is_ok_and(|r| r.is_ok());
        std::process::exit(if ok { 0 } else { 1 });
    }

    let cfg = load_config();
    if let Err(e) = std::fs::create_dir_all(&cfg.data_dir) {
        eprintln!(
            "sequencer: WARNING could not create DATA_DIR {}: {e}; saving will fail",
            cfg.data_dir.display()
        );
    }
    eprintln!("sequencer: saving beats under {}", cfg.data_dir.display());
    if cfg.openrouter_key.is_none() {
        eprintln!("sequencer: OPENROUTER_API_KEY not set; AI generation is disabled");
    } else {
        let ids: Vec<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
        eprintln!("sequencer: AI generation enabled with {}", ids.join(", "));
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], port()));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    eprintln!("sequencer: listening on http://{addr}");

    axum::serve(listener, build_router(AppState::new(cfg)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}
