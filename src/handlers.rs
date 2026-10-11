//! Page, config and saved-beat handlers.

use crate::AppState;
use crate::auth::{Viewer, db_failure, username_problem};
use crate::db::{MAX_BEATS, NewBeat};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

const INDEX_HTML: &str = include_str!("../static/index.html");
// 16, 32 and 48 px renders of static/icon/icon.svg
const FAVICON: &[u8] = include_bytes!("../static/favicon.ico");
const TOUCH_ICON: &[u8] = include_bytes!("../static/apple-touch-icon.png");

/// `{code, message}` JSON error, the shape the frontend reads.
pub fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

/// Turn axum's JSON rejections (bad JSON, wrong content type, too large) into our shape.
fn json_rejection(r: JsonRejection) -> Response {
    let status = r.status();
    let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
        "too_large"
    } else {
        "invalid_argument"
    };
    err(status, code, &r.body_text())
}

fn page(html: String) -> Response {
    (
        [
            ("content-type", "text/html; charset=UTF-8"),
            ("cache-control", "no-cache"),
            ("x-content-type-options", "nosniff"),
            ("x-frame-options", "DENY"),
        ],
        html,
    )
        .into_response()
}

pub async fn index() -> Response {
    page(INDEX_HTML.to_string())
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/<username>/<n>`: the app, opening that beat. The link preview (title and
/// description) is filled in server side so shared links unfurl with the beat's name.
pub async fn beat_page(
    State(state): State<AppState>,
    Path((username, num)): Path<(String, String)>,
) -> Response {
    let Some(num) = beat_num(&num) else {
        return not_found_page();
    };
    if username_problem(&username).is_some() && username != "local" {
        return not_found_page();
    }
    let Ok(Some(b)) = state.db.public_beat(username.clone(), num).await else {
        return not_found_page();
    };
    let name = b["name"].as_str().unwrap_or("A beat");
    let title = escape_html(&format!("{name} by {username}"));
    let desc = escape_html(&format!(
        "{} BPM. Play it, remix it and make your own on 16.aks.zone.",
        b["bpm"].as_f64().unwrap_or(0.0).round()
    ));
    page(
        INDEX_HTML
            .replacen(
                OG_TITLE,
                &format!(r#"<meta property="og:title" content="{title}">"#),
                1,
            )
            .replacen(
                OG_DESC,
                &format!(r#"<meta property="og:description" content="{desc}">"#),
                1,
            ),
    )
}

const OG_TITLE: &str = r#"<meta property="og:title" content="16.AKS.ZONE">"#;
const OG_DESC: &str = r#"<meta property="og:description" content="An 808-style drum machine and synth sequencer in your browser.">"#;

fn not_found_page() -> Response {
    let mut r = page(INDEX_HTML.to_string());
    *r.status_mut() = StatusCode::NOT_FOUND;
    r
}

/// Beat numbers are 1, 2, 3, ... (no leading zeros, so each beat has one address).
fn beat_num(s: &str) -> Option<i64> {
    if s.is_empty() || s.len() > 9 || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub async fn favicon() -> Response {
    icon("image/x-icon", FAVICON)
}

pub async fn touch_icon() -> Response {
    icon("image/png", TOUCH_ICON)
}

fn icon(kind: &'static str, bytes: &'static [u8]) -> Response {
    (
        [
            ("content-type", kind),
            ("cache-control", "public, max-age=86400"),
        ],
        bytes,
    )
        .into_response()
}

/// The commit this binary was built from ("dev" outside CI), to check a deploy is running the latest code.
pub const COMMIT: &str = match option_env!("GIT_SHA") {
    Some(s) if !s.is_empty() => s,
    _ => "dev",
};

pub async fn version() -> Response {
    (
        [("cache-control", "no-store")],
        Json(json!({ "version": env!("CARGO_PKG_VERSION"), "commit": COMMIT })),
    )
        .into_response()
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// What the frontend can do on this server.
pub async fn config(State(state): State<AppState>) -> Json<Value> {
    let models: Vec<Value> = state
        .cfg
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "label": m.label }))
        .collect();
    Json(json!({
        // "google": sign in with Google to save to an account and use AI; "none": one
        // built-in account, no sign-in
        "auth": if state.google.is_some() { "google" } else { "none" },
        "googleClientId": state.google.as_ref().map(|g| g.client_id.clone()),
        "save": true,
        "generate": state.cfg.openrouter_key.is_some() && !models.is_empty(),
        "aiDailyLimit": state.cfg.ai_daily_limit,
        "models": models,
        // generation can carry the spectrogram (every default model takes images)
        "images": true,
    }))
}

#[derive(Deserialize)]
pub struct BeatBody {
    #[serde(default)]
    name: String,
    beat: Value,
}

pub async fn list_beats(State(state): State<AppState>, viewer: Viewer) -> Response {
    let (user, username) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.db.list_beats(user.id).await {
        Ok(beats) => Json(json!({ "username": username, "beats": beats })).into_response(),
        Err(e) => db_failure(e),
    }
}

pub async fn get_beat(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Response {
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(num) = beat_num(&id) else {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "bad id");
    };
    match state.db.get_beat(user.id, num).await {
        Ok(Some(b)) => Json(b).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found", "no such beat"),
        Err(e) => db_failure(e),
    }
}

pub async fn create_beat(
    State(state): State<AppState>,
    viewer: Viewer,
    body: Result<Json<BeatBody>, JsonRejection>,
) -> Response {
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return json_rejection(r),
    };
    let nb = match NewBeat::new(&b.name, &b.beat, (None, None)) {
        Ok(nb) => nb,
        Err(e) => return db_failure(e),
    };
    match state.db.create_beats(user.id, vec![nb]).await {
        Ok(mut docs) => (StatusCode::CREATED, Json(docs.remove(0))).into_response(),
        Err(e) => db_failure(e),
    }
}

#[derive(Deserialize)]
pub struct ImportItem {
    #[serde(default)]
    name: String,
    beat: Value,
    #[serde(default, rename = "createdAt")]
    created_at: Option<i64>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<i64>,
}

#[derive(Deserialize)]
pub struct ImportBody {
    beats: Vec<ImportItem>,
}

/// Move a browser's saved beats into the account in one go (on sign-in). They're added
/// after whatever the account already has, in the order given, keeping their dates.
pub async fn import_beats(
    State(state): State<AppState>,
    viewer: Viewer,
    body: Result<Json<ImportBody>, JsonRejection>,
) -> Response {
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return json_rejection(r),
    };
    if b.beats.len() as i64 > MAX_BEATS {
        return err(
            StatusCode::INSUFFICIENT_STORAGE,
            "quota_exceeded",
            "too many saved beats",
        );
    }
    let mut items = vec![];
    for it in b.beats {
        match NewBeat::new(&it.name, &it.beat, (it.created_at, it.updated_at)) {
            Ok(nb) => items.push(nb),
            Err(e) => return db_failure(e),
        }
    }
    match state.db.create_beats(user.id, items).await {
        Ok(docs) => Json(json!({ "beats": docs })).into_response(),
        Err(e) => db_failure(e),
    }
}

pub async fn update_beat(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    body: Result<Json<BeatBody>, JsonRejection>,
) -> Response {
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(num) = beat_num(&id) else {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "bad id");
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return json_rejection(r),
    };
    let nb = match NewBeat::new(&b.name, &b.beat, (None, None)) {
        Ok(nb) => nb,
        Err(e) => return db_failure(e),
    };
    match state.db.update_beat(user.id, num, nb).await {
        Ok(doc) => Json(doc).into_response(),
        Err(e) => db_failure(e),
    }
}

pub async fn delete_beat(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Response {
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(num) = beat_num(&id) else {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "bad id");
    };
    match state.db.delete_beat(user.id, num).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found", "no such beat"),
        Err(e) => db_failure(e),
    }
}

/// Anyone's beat, by its address. Beats are public to anyone with the link.
pub async fn public_beat(
    State(state): State<AppState>,
    Path((username, id)): Path<(String, String)>,
) -> Response {
    let Some(num) = beat_num(&id) else {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "bad id");
    };
    match state.db.public_beat(username.clone(), num).await {
        Ok(Some(mut b)) => {
            b["username"] = json!(username);
            ([("cache-control", "no-cache")], Json(b)).into_response()
        }
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found", "no such beat"),
        Err(e) => db_failure(e),
    }
}
