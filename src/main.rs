//! 16.AKS.ZONE: an 808-style drum machine + synth sequencer.
//!
//! The whole frontend is one HTML file baked into the binary. The server adds what a
//! static page can't do on its own: Google sign-in, saved beats with a public address
//! each (`/<username>/<n>`, in SQLite under `DATA_DIR`), and AI beat generation (a
//! streaming proxy to OpenRouter, so the API key stays server side, metered per account).

mod ai;
mod auth;
mod db;
mod handlers;
#[cfg(test)]
mod tests;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
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
/// Bigger models, offered only to accounts whose own AI limit is over `PREMIUM_MIN_LIMIT`.
pub const DEFAULT_PREMIUM_MODELS: &str = "~openai/gpt-astra-latest=GPT Astra";
pub const PREMIUM_MIN_LIMIT: i64 = 10;
/// Models that get OpenRouter's web search tool, so they can look things up (a song, a genre).
pub const DEFAULT_SEARCH_MODELS: &str = "~google/gemini-flash-latest";

/// Largest request body we accept (a 4-page beat with several synths is ~10 kB; a
/// generation prompt with "build on current beat" is ~20 kB).
pub const BODY_LIMIT: usize = 256 * 1024;
/// `/api/generate` can also carry the beat's spectrogram as a base64 image.
pub const GENERATE_BODY_LIMIT: usize = 2 * 1024 * 1024;
/// Moving a browser's saved beats into an account sends them all at once.
pub const IMPORT_BODY_LIMIT: usize = 16 * 1024 * 1024;
pub const DEFAULT_AI_DAILY_LIMIT: i64 = 10;

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
    /// Offered on top of `models` to accounts with an `ai_limit` over `PREMIUM_MIN_LIMIT`.
    pub premium_models: Vec<Model>,
    /// Model ids that can search the web while they write.
    pub search_models: Vec<String>,
    /// Turns on Google sign-in. Without it, there's one built-in `local` account.
    pub google_client_id: Option<String>,
    pub google_certs_url: String,
    /// AI generations per account per day (UTC), unless the account has its own limit.
    pub ai_daily_limit: i64,
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
        premium_models: parse_models(
            &var("OPENROUTER_PREMIUM_MODELS").unwrap_or_else(|| DEFAULT_PREMIUM_MODELS.into()),
        ),
        search_models: parse_models(
            &var("OPENROUTER_SEARCH_MODELS").unwrap_or_else(|| DEFAULT_SEARCH_MODELS.into()),
        )
        .into_iter()
        .map(|m| m.id)
        .collect(),
        google_client_id: var("GOOGLE_CLIENT_ID"),
        google_certs_url: var("GOOGLE_CERTS_URL").unwrap_or_else(|| auth::DEFAULT_CERTS_URL.into()),
        ai_daily_limit: var("AI_DAILY_LIMIT")
            .and_then(|s| s.parse().ok())
            .filter(|&n: &i64| n >= 0)
            .unwrap_or(DEFAULT_AI_DAILY_LIMIT),
    }
}

/// Shared, cheaply-cloneable application state. `reqwest::Client` is internally `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: db::Db,
    pub client: reqwest::Client,
    /// Set when sign-in is on.
    pub google: Option<Arc<auth::Google>>,
    /// The built-in account everyone acts as when sign-in is off.
    pub local_user: Option<db::User>,
}

impl AppState {
    /// Opens (or creates) `$DATA_DIR/sequencer.db`.
    pub async fn new(cfg: Config) -> Result<Self, String> {
        std::fs::create_dir_all(&cfg.data_dir)
            .map_err(|e| format!("couldn't create DATA_DIR {}: {e}", cfg.data_dir.display()))?;
        let path = cfg.data_dir.join("sequencer.db");
        let db =
            db::Db::open(&path).map_err(|e| format!("couldn't open {}: {e}", path.display()))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");
        let google = cfg
            .google_client_id
            .clone()
            .map(|id| Arc::new(auth::Google::new(id, cfg.google_certs_url.clone())));
        let local_user = match google {
            Some(_) => None,
            None => Some(db.local_user().await.map_err(|e| format!("{e:?}"))?),
        };
        Ok(AppState {
            cfg: Arc::new(cfg),
            db,
            client,
            google,
            local_user,
        })
    }

    /// Today's AI allowance for an account. Unmetered without sign-in.
    pub fn ai_limit(&self, u: &db::User) -> Option<i64> {
        self.google.as_ref()?;
        Some(u.ai_limit.unwrap_or(self.cfg.ai_daily_limit))
    }

    /// The premium models this account gets: those whose own limit was raised over
    /// `PREMIUM_MIN_LIMIT` (by `set-ai-limit` or in the database), with or without sign-in.
    pub fn extra_models(&self, u: &db::User) -> &[Model] {
        if u.ai_limit.is_some_and(|l| l > PREMIUM_MIN_LIMIT) {
            &self.cfg.premium_models
        } else {
            &[]
        }
    }

    /// Can this account generate with this model?
    pub fn model_ok(&self, u: &db::User, id: &str) -> bool {
        self.cfg
            .models
            .iter()
            .chain(self.extra_models(u))
            .any(|m| m.id == id)
    }
}

/// Build the app router. Kept public so tests can drive it via `oneshot`.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(handlers::index))
        .route("/favicon.ico", get(handlers::favicon))
        .route("/apple-touch-icon.png", get(handlers::touch_icon))
        .route("/healthz", get(handlers::healthz))
        .route("/api/version", get(handlers::version))
        .route("/api/config", get(handlers::config))
        .route("/api/me", get(auth::me))
        .route("/api/auth/google", post(auth::google_sign_in))
        .route("/api/auth/signout", post(auth::sign_out))
        .route("/api/auth/username", post(auth::set_username))
        .route("/api/usernames/{name}", get(auth::username_check))
        .route(
            "/api/beats",
            get(handlers::list_beats).post(handlers::create_beat),
        )
        .route(
            "/api/beats/import",
            post(handlers::import_beats).layer(DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
        )
        .route(
            "/api/beats/{id}",
            get(handlers::get_beat)
                .put(handlers::update_beat)
                .delete(handlers::delete_beat),
        )
        .route("/api/u/{username}/{id}", get(handlers::public_beat))
        .route("/{username}/{id}", get(handlers::beat_page))
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
    // `set-ai-limit <username> <n|default>`: give one account its own daily AI limit
    if std::env::args().nth(1).as_deref() == Some("set-ai-limit") {
        std::process::exit(set_ai_limit(cfg).await);
    }
    eprintln!(
        "sequencer: saving to {}",
        cfg.data_dir.join("sequencer.db").display()
    );
    match &cfg.google_client_id {
        Some(_) => eprintln!(
            "sequencer: Google sign-in on, {} AI generations per account per day",
            cfg.ai_daily_limit
        ),
        None => eprintln!(
            "sequencer: GOOGLE_CLIENT_ID not set; no sign-in, everyone shares one local account"
        ),
    }
    if cfg.openrouter_key.is_none() {
        eprintln!("sequencer: OPENROUTER_API_KEY not set; AI generation is disabled");
    } else {
        let ids: Vec<&str> = cfg.models.iter().map(|m| m.id.as_str()).collect();
        eprintln!("sequencer: AI generation enabled with {}", ids.join(", "));
        if !cfg.premium_models.is_empty() {
            let ids: Vec<&str> = cfg.premium_models.iter().map(|m| m.id.as_str()).collect();
            eprintln!(
                "sequencer: accounts with an AI limit over {PREMIUM_MIN_LIMIT} also get {}",
                ids.join(", ")
            );
        }
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], port()));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    eprintln!(
        "sequencer: listening on http://{addr} (commit {})",
        handlers::COMMIT
    );

    let state = AppState::new(cfg).await.unwrap_or_else(|e| {
        eprintln!("sequencer: {e}");
        std::process::exit(1)
    });
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

async fn set_ai_limit(cfg: Config) -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let limit = match args.get(1).map(String::as_str) {
        Some("default") => None,
        Some(n) => match n.parse::<i64>() {
            Ok(n) if n >= 0 => Some(n),
            _ => {
                eprintln!("usage: sequencer set-ai-limit <username> <n|default>");
                return 2;
            }
        },
        None => {
            eprintln!("usage: sequencer set-ai-limit <username> <n|default>");
            return 2;
        }
    };
    let path = cfg.data_dir.join("sequencer.db");
    let db = match db::Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("couldn't open {}: {e}", path.display());
            return 1;
        }
    };
    match db.set_ai_limit(args[0].clone(), limit).await {
        Ok(true) => {
            match limit {
                Some(n) => println!("{} now gets {n} AI generations a day", args[0]),
                None => println!(
                    "{} is back to the default ({} a day)",
                    args[0], cfg.ai_daily_limit
                ),
            }
            0
        }
        Ok(false) => {
            eprintln!("no account named {}", args[0]);
            1
        }
        Err(e) => {
            eprintln!("{e:?}");
            1
        }
    }
}
