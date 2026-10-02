use crate::ai::{Event, parse_line};
use crate::{AppState, BODY_LIMIT, Config, Model, build_router, parse_models};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tower::ServiceExt;

fn temp_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "sequencer-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn cfg(dir: PathBuf, key: Option<&str>, base: &str) -> Config {
    Config {
        data_dir: dir,
        openrouter_key: key.map(str::to_string),
        openrouter_base: base.to_string(),
        models: vec![Model {
            id: "test/model".into(),
            label: "Test".into(),
        }],
    }
}

fn app(dir: PathBuf) -> Router {
    build_router(AppState::new(cfg(dir, None, "http://127.0.0.1:1")))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn index_serves_app() {
    let app = app(temp_dir());
    let resp = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&html).contains("AK-16"));
}

#[tokio::test]
async fn serves_the_icons() {
    for (path, kind, magic) in [
        ("/favicon.ico", "image/x-icon", &[0u8, 0, 1, 0][..]),
        ("/apple-touch-icon.png", "image/png", &b"\x89PNG"[..]),
    ] {
        let resp = app(temp_dir())
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(resp.headers()["content-type"], kind);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(body.starts_with(magic), "{path}");
    }
}

#[tokio::test]
async fn config_reports_generation_off_without_key() {
    let (s, v) = call(&app(temp_dir()), "GET", "/api/config", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["generate"], false);
    assert_eq!(v["save"], true);
    assert_eq!(v["auth"], "none");
    assert_eq!(v["models"][0]["id"], "test/model");
}

#[tokio::test]
async fn beats_crud_round_trip() {
    let dir = temp_dir();
    let app = app(dir.clone());

    let (s, v) = call(&app, "GET", "/api/beats", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["beats"], json!([]));

    let (s, a) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"  First  ","beat":{"bpm":90}})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let id = a["id"].as_str().unwrap().to_string();
    assert_eq!(a["name"], "First");
    assert!(dir.join("local/beats").join(format!("{id}.json")).exists());

    let (_, b) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"","beat":{"bpm":120}})),
    )
    .await;
    assert_eq!(b["name"], "Untitled beat");

    // Update keeps createdAt, bumps updatedAt, and moves it to the top of the list.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (s, u) = call(
        &app,
        "PUT",
        &format!("/api/beats/{id}"),
        Some(json!({"name":"Renamed","beat":{"bpm":95}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(u["createdAt"], a["createdAt"]);
    assert!(u["updatedAt"].as_u64() > a["updatedAt"].as_u64());

    let (_, v) = call(&app, "GET", "/api/beats", None).await;
    let beats = v["beats"].as_array().unwrap();
    assert_eq!(beats.len(), 2);
    assert_eq!(beats[0]["name"], "Renamed");
    assert_eq!(beats[0]["beat"]["bpm"], 95);

    let (s, _) = call(&app, "DELETE", &format!("/api/beats/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "DELETE", &format!("/api/beats/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, v) = call(&app, "GET", "/api/beats", None).await;
    assert_eq!(v["beats"].as_array().unwrap().len(), 1);

    // A fresh server over the same dir sees the same beats.
    let (_, v) = call(&self::app(dir), "GET", "/api/beats", None).await;
    assert_eq!(v["beats"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_bad_ids_and_bodies() {
    let app = app(temp_dir());
    let (s, _) = call(&app, "PUT", "/api/beats/..%2Fetc", Some(json!({"beat":{}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&app, "PUT", "/api/beats/ABC", Some(json!({"beat":{}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = call(&app, "DELETE", "/api/beats/a.b", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"x","beat":[1,2]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "invalid_argument");
    let (s, _) = call(&app, "POST", "/api/beats", Some(json!({"name":"x"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn rejects_oversize_body() {
    let big = "x".repeat(BODY_LIMIT + 1);
    let (s, v) = call(
        &app(temp_dir()),
        "POST",
        "/api/beats",
        Some(json!({"name":"x","beat":{"pad":big}})),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(v["code"], "too_large");
}

#[tokio::test]
async fn generate_needs_key_and_known_model() {
    let (s, v) = call(
        &app(temp_dir()),
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"hi"})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["code"], "not_configured");

    let keyed = build_router(AppState::new(cfg(
        temp_dir(),
        Some("k"),
        "http://127.0.0.1:1",
    )));
    let (s, _) = call(
        &keyed,
        "POST",
        "/api/generate",
        Some(json!({"model":"openai/expensive","prompt":"hi"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[test]
fn parses_model_list() {
    let m = parse_models(" a/b=Model B , c/d ,, e/f= ");
    assert_eq!(m.len(), 3);
    assert_eq!((m[0].id.as_str(), m[0].label.as_str()), ("a/b", "Model B"));
    assert_eq!((m[1].id.as_str(), m[1].label.as_str()), ("c/d", "c/d"));
    assert_eq!(m[2].label, "e/f");
}

#[test]
fn parses_upstream_sse_lines() {
    assert_eq!(parse_line(": OPENROUTER PROCESSING"), vec![Event::Ignore]);
    assert_eq!(parse_line("data: [DONE]"), vec![Event::Done]);
    assert_eq!(
        parse_line(r#"data: {"choices":[{"delta":{"content":"{\"bpm\""}}]}"#),
        vec![Event::Text("{\"bpm\"".into())]
    );
    assert_eq!(
        parse_line(r#"data: {"choices":[{"delta":{"content":""},"finish_reason":"length"}]}"#),
        vec![Event::Finish("length".into())]
    );
    assert_eq!(
        parse_line(r#"data: {"error":{"message":"boom"}}"#),
        vec![Event::Error("boom".into())]
    );
}

/// End to end against a fake OpenRouter: the text deltas come through in order and
/// the stream ends with `done`, carrying `truncated` from `finish_reason`.
#[tokio::test]
async fn generate_streams_from_upstream() {
    let mock = Router::new().route(
        "/chat/completions",
        post(|body: String| async move {
            let v: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["model"], "test/model");
            assert_eq!(v["stream"], true);
            let sse = concat!(
                ": OPENROUTER PROCESSING\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"name\\\":\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"\\\"Hi\\\"}\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            ([("content-type", "text/event-stream")], sse)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let app = build_router(AppState::new(cfg(temp_dir(), Some("k"), &base)));
    let resp = app
        .oneshot(
            Request::post("/api/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"test/model","prompt":"a beat"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let events: Vec<Value> = String::from_utf8_lossy(&body)
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect();
    let text: String = events.iter().filter_map(|e| e["t"].as_str()).collect();
    assert_eq!(text, r#"{"name":"Hi"}"#);
    assert_eq!(
        events.last().unwrap(),
        &json!({"done":true,"truncated":false})
    );
}

/// Upstream HTTP errors map to codes the UI understands.
#[tokio::test]
async fn generate_maps_upstream_errors() {
    let mock = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                axum::Json(json!({"error":{"message":"slow down"}})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

    let app = build_router(AppState::new(cfg(temp_dir(), Some("k"), &base)));
    let (s, v) = call(
        &app,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"x"})),
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(v["code"], "rate_limited");
}

/// A spectrogram rides along as an image part, and a request that big is fine on this route
/// (it's over the 256 kB limit the beats routes keep).
#[tokio::test]
async fn generate_forwards_the_image() {
    let mock = Router::new().route(
        "/chat/completions",
        post(|body: String| async move {
            let v: Value = serde_json::from_str(&body).unwrap();
            let c = &v["messages"][0]["content"];
            assert_eq!(c[0]["type"], "image_url");
            assert!(c[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
            assert_eq!(c[1], json!({"type":"text","text":"a beat"}));
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            ([("content-type", "text/event-stream")], sse)
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let app = build_router(AppState::new(cfg(temp_dir(), Some("k"), &base)));
    let image = format!(
        "data:image/jpeg;base64,{}",
        "A".repeat(BODY_LIMIT + 100_000)
    );
    let resp = app
        .oneshot(
            Request::post("/api/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"test/model","prompt":"a beat","image":image}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains(r#"{"t":"ok"}"#));
}

#[tokio::test]
async fn generate_checks_the_image() {
    let keyed = build_router(AppState::new(cfg(
        temp_dir(),
        Some("k"),
        "http://127.0.0.1:1",
    )));
    for bad in [
        "https://example.com/x.jpg",
        "data:text/html;base64,AAAA",
        "data:image/jpeg,AAAA",
    ] {
        let (s, v) = call(
            &keyed,
            "POST",
            "/api/generate",
            Some(json!({"model":"test/model","prompt":"x","image":bad})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(v["code"], "invalid_argument");
    }
    let huge = format!(
        "data:image/png;base64,{}",
        "A".repeat(crate::GENERATE_BODY_LIMIT)
    );
    let (s, _) = call(
        &keyed,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"x","image":huge})),
    )
    .await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

/// A model that can't take images comes back as `image_rejected`, so the browser can retry
/// without the picture.
#[tokio::test]
async fn generate_reports_image_rejected() {
    let mock = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                StatusCode::NOT_FOUND,
                axum::Json(
                    json!({"error":{"message":"No endpoints found that support image input"}}),
                ),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let app = build_router(AppState::new(cfg(temp_dir(), Some("k"), &base)));
    let (s, v) = call(
        &app,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"x","image":"data:image/jpeg;base64,AAAAAAAA"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["code"], "image_rejected");
    // without an image the same failure is an ordinary upstream error
    let (s, v) = call(
        &app,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"x"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert_eq!(v["code"], "upstream_error");
}
