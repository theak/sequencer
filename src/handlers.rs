//! Page, config and saved-beat handlers.

use crate::AppState;
use crate::store::{Owner, StoreError};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

const INDEX_HTML: &str = include_str!("../static/index.html");

/// `{code, message}` JSON error, the shape the frontend reads.
pub fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({ "code": code, "message": message }))).into_response()
}

impl IntoResponse for StoreError {
    fn into_response(self) -> Response {
        match self {
            StoreError::BadRequest(m) => err(StatusCode::BAD_REQUEST, "invalid_argument", m),
            StoreError::Full => err(
                StatusCode::INSUFFICIENT_STORAGE,
                "quota_exceeded",
                "too many saved beats",
            ),
            StoreError::Io(e) => {
                eprintln!("sequencer: storage error: {e}");
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "storage_error",
                    "couldn't read or write the data dir",
                )
            }
        }
    }
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

pub async fn index() -> Response {
    (
        [
            ("content-type", "text/html; charset=UTF-8"),
            ("cache-control", "no-cache"),
        ],
        INDEX_HTML,
    )
        .into_response()
}

pub async fn healthz() -> &'static str {
    "ok"
}

/// What the frontend can do on this server. `auth` is `"none"` until sign-in lands.
pub async fn config(State(state): State<AppState>) -> Json<Value> {
    let models: Vec<Value> = state
        .cfg
        .models
        .iter()
        .map(|m| json!({ "id": m.id, "label": m.label }))
        .collect();
    Json(json!({
        "auth": "none",
        "save": true,
        "generate": state.cfg.openrouter_key.is_some() && !models.is_empty(),
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

pub async fn list_beats(State(state): State<AppState>, owner: Owner) -> Response {
    match state.store.list(&owner).await {
        Ok(beats) => Json(json!({ "beats": beats })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn create_beat(
    State(state): State<AppState>,
    owner: Owner,
    body: Result<Json<BeatBody>, JsonRejection>,
) -> Response {
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return json_rejection(r),
    };
    match state.store.save(&owner, None, &b.name, b.beat).await {
        Ok(doc) => (StatusCode::CREATED, Json(doc)).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn update_beat(
    State(state): State<AppState>,
    owner: Owner,
    Path(id): Path<String>,
    body: Result<Json<BeatBody>, JsonRejection>,
) -> Response {
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return json_rejection(r),
    };
    match state.store.save(&owner, Some(&id), &b.name, b.beat).await {
        Ok(doc) => Json(doc).into_response(),
        Err(e) => e.into_response(),
    }
}

pub async fn delete_beat(
    State(state): State<AppState>,
    owner: Owner,
    Path(id): Path<String>,
) -> Response {
    match state.store.delete(&owner, &id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found", "no such beat"),
        Err(e) => e.into_response(),
    }
}
