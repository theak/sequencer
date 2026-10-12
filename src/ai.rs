//! AI beat generation: a streaming proxy to OpenRouter's chat completions API.
//!
//! The browser POSTs `{model, prompt, image?}` and gets back Server-Sent Events:
//!   `data: {"t":"<text delta>"}`                    as the model writes
//!   `data: {"done":true,"truncated":false}`         when it finishes
//!   `data: {"error":"<code>","message":"<text>"}`   if it fails mid-stream
//! Failures before the stream starts are plain `{code, message}` JSON errors.
//!
//! `image` is an optional `data:image/...;base64,` URL (the beat's spectrogram). It goes to
//! the model as an image part next to the prompt. If the model won't take images, the call
//! fails with `image_rejected` and the browser retries without one.
//!
//! The API key never leaves the server, and only allowlisted models can be requested
//! (plus the premium ones, for accounts whose AI limit was raised over 10).
//!
//! Models in `search_models` get OpenRouter's `openrouter:web_search` server tool:
//! OpenRouter runs the searches (Gemini uses Google's own), the model decides when to
//! search, and only its text streams back. If OpenRouter turns the tool down, the request
//! goes again without it.
//! With sign-in on, generating needs an account, and each account gets `AI_DAILY_LIMIT`
//! generations a day. A request that fails before the model writes anything doesn't count.

use crate::AppState;
use crate::auth::{Viewer, db_failure};
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
/// A spectrogram JPEG is ~100-300 kB; anything much bigger isn't one of ours.
const MAX_IMAGE_BYTES: usize = 1_500_000;
/// The prompt's text. The app's own prompt, with the biggest beat it can build on, is
/// well under this; the cap stops someone sending a huge (and costly) prompt by hand.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// Is this a base64 data URL for an image type the models take?
pub fn valid_image(url: &str) -> bool {
    url.len() <= MAX_IMAGE_BYTES
        && ["image/jpeg", "image/png", "image/webp"].iter().any(|t| {
            url.strip_prefix("data:")
                .and_then(|r| r.strip_prefix(t))
                .is_some_and(|r| r.starts_with(";base64,") && r.len() > 8)
        })
}

#[derive(Deserialize)]
pub struct GenerateBody {
    model: String,
    prompt: String,
    #[serde(default)]
    image: Option<String>,
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
    viewer: Viewer,
    body: Result<Json<GenerateBody>, JsonRejection>,
) -> Response {
    let Some(key) = state.cfg.openrouter_key.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "AI generation is off on this server. Set OPENROUTER_API_KEY to turn it on.",
        );
    };
    let (user, _) = match viewer.member() {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return err(r.status(), "invalid_argument", &r.body_text()),
    };
    if !state.model_ok(&user, &b.model) {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "unknown model");
    }
    if b.prompt.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "invalid_argument", "empty prompt");
    }
    if b.prompt.len() > MAX_PROMPT_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "The prompt is over 64 KB.",
        );
    }
    if b.image.as_deref().is_some_and(|i| !valid_image(i)) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_argument",
            "image must be a JPEG, PNG or WebP data URL under 1.5 MB",
        );
    }
    if let Some(limit) = state.ai_limit(&user) {
        match state.db.ai_take(user.id, limit).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return err(
                    StatusCode::TOO_MANY_REQUESTS,
                    "daily_limit",
                    &format!("That's all {limit} AI beats for today. More tomorrow."),
                );
            }
            Err(e) => return db_failure(e),
        }
    }
    // hand the generation back if it fails before the model writes anything
    let refund = || async {
        if state.ai_limit(&user).is_some() {
            let _ = state.db.ai_refund(user.id).await;
        }
    };
    let content = match &b.image {
        Some(url) => json!([
            { "type": "image_url", "image_url": { "url": url } },
            { "type": "text", "text": b.prompt },
        ]),
        None => json!(b.prompt),
    };

    let send = |search: bool| {
        let mut body = json!({
            "model": b.model,
            "stream": true,
            "max_tokens": MAX_TOKENS,
            "messages": [{ "role": "user", "content": content }],
        });
        if search {
            body["tools"] = json!([{ "type": "openrouter:web_search" }]);
        }
        state
            .client
            .post(format!("{}/chat/completions", state.cfg.openrouter_base))
            .bearer_auth(&key)
            .header("X-Title", "16.AKS.ZONE")
            .timeout(UPSTREAM_TIMEOUT)
            .json(&body)
            .send()
    };
    let search = state.cfg.search_models.contains(&b.model);
    let mut sent = send(search).await;
    // a search the model or route can't do shouldn't cost the beat: try once without it
    if search
        && let Ok(r) = &sent
        && matches!(r.status().as_u16(), 400 | 404 | 422)
    {
        eprintln!(
            "sequencer: OpenRouter {} with web search; retrying without it",
            r.status()
        );
        sent = send(false).await;
    }
    let mut resp = match sent {
        Ok(r) => r,
        Err(e) => {
            eprintln!("sequencer: OpenRouter request failed: {e}");
            refund().await;
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
        refund().await;
        // OpenRouter answers 404 "no endpoints support image input" (or a 400/415/422)
        // when the model can't see images: say so, so the browser can retry without one
        if b.image.is_some() && matches!(status.as_u16(), 400 | 404 | 415 | 422) {
            let msg = body["error"]["message"].as_str().unwrap_or("");
            eprintln!("sequencer: OpenRouter {status} with an image: {msg}");
            return err(
                StatusCode::BAD_REQUEST,
                "image_rejected",
                "This model can't take the spectrogram image.",
            );
        }
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
