//! AI beat generation: a streaming proxy to OpenRouter's chat completions API.
//!
//! The browser POSTs `{model, prompt}` and gets back Server-Sent Events:
//!   `data: {"t":"<text delta>"}`                    as the model writes
//!   `data: {"done":true,"truncated":false}`         when it finishes
//!   `data: {"error":"<code>","message":"<text>"}`   if it fails mid-stream
//! Failures before the stream starts are plain `{code, message}` JSON errors.
//!
//! The API key never leaves the server, and only allowlisted models can be requested.

use crate::AppState;
use crate::handlers::err;
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Room for a 4-page beat with several synth channels, plus reasoning tokens.
const MAX_TOKENS: u32 = 16000;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Deserialize)]
pub struct GenerateBody {
    model: String,
    prompt: String,
}

fn sse(v: Value) -> Bytes {
    Bytes::from(format!("data: {v}\n\n"))
}

/// What one upstream SSE `data:` payload means for us.
#[derive(Debug, PartialEq)]
pub enum Event {
    Text(String),
    Finish(String),
    Error(String),
    Done,
    Ignore,
}

/// Parse one line of OpenRouter's SSE stream. Comment lines (`: OPENROUTER PROCESSING`)
/// and blanks are ignored.
pub fn parse_line(line: &str) -> Vec<Event> {
    let Some(data) = line.strip_prefix("data:") else {
        return vec![Event::Ignore];
    };
    let data = data.trim();
    if data == "[DONE]" {
        return vec![Event::Done];
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return vec![Event::Ignore];
    };
    if let Some(e) = v.get("error") {
        let msg = e["message"]
            .as_str()
            .unwrap_or("upstream error")
            .to_string();
        return vec![Event::Error(msg)];
    }
    let choice = &v["choices"][0];
    let mut out = vec![];
    if let Some(t) = choice["delta"]["content"].as_str()
        && !t.is_empty()
    {
        out.push(Event::Text(t.to_string()));
    }
    if let Some(f) = choice["finish_reason"].as_str() {
        out.push(Event::Finish(f.to_string()));
    }
    if out.is_empty() {
        out.push(Event::Ignore);
    }
    out
}

/// Map a non-2xx OpenRouter response to our error codes (the ones the UI knows).
fn upstream_failure(status: reqwest::StatusCode, body: &Value) -> Response {
    let msg = body["error"]["message"].as_str().unwrap_or("").to_string();
    eprintln!("sequencer: OpenRouter {status}: {msg}");
    match status.as_u16() {
        401 | 403 => err(
            StatusCode::BAD_GATEWAY,
            "upstream_auth",
            "OpenRouter rejected the API key. Check OPENROUTER_API_KEY.",
        ),
        402 => err(
            StatusCode::PAYMENT_REQUIRED,
            "no_credits",
            "The OpenRouter account is out of credits.",
        ),
        429 => err(StatusCode::TOO_MANY_REQUESTS, "rate_limited", &msg),
        400 | 413 => err(StatusCode::BAD_REQUEST, "upstream_rejected", &msg),
        _ => err(StatusCode::BAD_GATEWAY, "upstream_error", &msg),
    }
}

pub async fn generate(
    State(state): State<AppState>,
    body: Result<Json<GenerateBody>, JsonRejection>,
) -> Response {
    let Some(key) = state.cfg.openrouter_key.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "AI generation is off on this server. Set OPENROUTER_API_KEY to turn it on.",
        );
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return err(r.status(), "invalid_argument", &r.body_text()),
    };
    if !state.cfg.models.iter().any(|m| m.id == b.model) {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "unknown model");
    }
    if b.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "empty prompt");
    }

    let req = state
        .client
        .post(format!("{}/chat/completions", state.cfg.openrouter_base))
        .bearer_auth(key)
        .header("X-Title", "AK-16 Sequencer")
        .timeout(UPSTREAM_TIMEOUT)
        .json(&json!({
            "model": b.model,
            "stream": true,
            "max_tokens": MAX_TOKENS,
            "messages": [{ "role": "user", "content": b.prompt }],
        }));
    let mut resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("sequencer: OpenRouter request failed: {e}");
            return err(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "couldn't reach OpenRouter",
            );
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        return upstream_failure(status, &body);
    }

    // Relay the upstream stream through a channel. When the browser disconnects (Stop
    // button), the receiver drops, `send` fails, and dropping `resp` aborts upstream.
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(64);
    tokio::spawn(async move {
        let mut buf: Vec<u8> = vec![];
        let mut finish: Option<String> = None;
        let mut failed: Option<String> = None;
        'read: loop {
            let chunk = match resp.chunk().await {
                Ok(Some(c)) => c,
                Ok(None) => break,
                Err(e) => {
                    failed = Some(format!("stream interrupted: {e}"));
                    break;
                }
            };
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&line);
                for ev in parse_line(line.trim_end()) {
                    match ev {
                        Event::Text(t) => {
                            if tx.send(Ok(sse(json!({ "t": t })))).await.is_err() {
                                return;
                            }
                        }
                        Event::Finish(f) => finish = Some(f),
                        Event::Error(m) => {
                            failed = Some(m);
                            break 'read;
                        }
                        Event::Done => break 'read,
                        Event::Ignore => {}
                    }
                }
            }
        }
        let last = match (failed, finish.as_deref()) {
            (Some(m), _) => json!({ "error": "upstream_error", "message": m }),
            (None, Some("content_filter")) => json!({ "error": "refused" }),
            (None, f) => json!({ "done": true, "truncated": f == Some("length") }),
        };
        let _ = tx.send(Ok(sse(last))).await;
    });

    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
            ("x-accel-buffering", "no"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}
