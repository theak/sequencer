use crate::ai::{Event, parse_line};
use crate::{AppState, BODY_LIMIT, Config, Model, build_router, parse_models};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
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
        google_client_id: None,
        google_certs_url: "http://127.0.0.1:1".into(),
        ai_daily_limit: 10,
    }
}

async fn router(c: Config) -> Router {
    build_router(AppState::new(c).await.unwrap())
}

/// A server without sign-in: everything is the built-in `local` account.
async fn app(dir: PathBuf) -> Router {
    router(cfg(dir, None, "http://127.0.0.1:1")).await
}

async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> axum::response::Response {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    app.clone().oneshot(req.body(body).unwrap()).await.unwrap()
}

async fn as_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    as_json(send(app, method, uri, body, None).await).await
}

async fn call_as(
    app: &Router,
    cookie: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    as_json(send(app, method, uri, body, Some(cookie)).await).await
}

async fn serve(mock: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    base
}

// ---- Google sign-in, with a throwaway RSA key standing in for Google's ----

const TEST_KEY: &[u8] = include_bytes!("testdata/google-test-key.pk8");
const TEST_JWKS: &str = include_str!("testdata/google-test-jwks.json");
const CLIENT_ID: &str = "test-client.apps.googleusercontent.com";

fn now_s() -> i64 {
    crate::db::now_ms() / 1000
}

/// An ID token like Google's, signed with the test key. `tweak` edits the claims.
fn id_token(sub: &str, tweak: impl FnOnce(&mut Value)) -> String {
    let mut claims = json!({
        "iss": "https://accounts.google.com", "aud": CLIENT_ID, "sub": sub,
        "email": format!("{sub}@example.com"), "iat": now_s(), "exp": now_s() + 3600,
    });
    tweak(&mut claims);
    let head = B64.encode(json!({"alg":"RS256","kid":"test-kid","typ":"JWT"}).to_string());
    let msg = format!("{head}.{}", B64.encode(claims.to_string()));
    let key = ring::signature::RsaKeyPair::from_pkcs8(TEST_KEY).unwrap();
    let mut sig = vec![0; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        msg.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{msg}.{}", B64.encode(sig))
}

/// A server with sign-in on, checking tokens against the test key.
async fn google_app(dir: PathBuf, key: Option<&str>, base: &str) -> Router {
    let certs = serve(Router::new().route(
        "/certs",
        get(|| async { ([("content-type", "application/json")], TEST_JWKS) }),
    ))
    .await;
    let mut c = cfg(dir, key, base);
    c.google_client_id = Some(CLIENT_ID.into());
    c.google_certs_url = format!("{certs}/certs");
    router(c).await
}

/// Sign in and return the session cookie (`sq_session=...`).
async fn sign_in(app: &Router, sub: &str) -> (String, Value) {
    let resp = send(
        app,
        "POST",
        "/api/auth/google",
        Some(json!({"credential": id_token(sub, |_| {})})),
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let set = resp.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(
        set.contains("HttpOnly") && set.contains("Secure") && set.contains("SameSite=Lax"),
        "{set}"
    );
    let cookie = set.split(';').next().unwrap().to_string();
    let (_, me) = as_json(resp).await;
    (cookie, me)
}

/// Sign in and pick a username.
async fn member(app: &Router, sub: &str, username: &str) -> String {
    let (cookie, _) = sign_in(app, sub).await;
    let (s, me) = call_as(
        app,
        &cookie,
        "POST",
        "/api/auth/username",
        Some(json!({"username": username})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    cookie
}

#[tokio::test]
async fn index_serves_app() {
    let app = app(temp_dir()).await;
    let resp = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let html = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&html).contains("16<b>.</b>AKS<b>.</b>ZONE"));
}

#[tokio::test]
async fn version_reports_the_commit() {
    let resp = app(temp_dir())
        .await
        .oneshot(Request::get("/api/version").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["commit"], crate::handlers::COMMIT);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn serves_the_icons() {
    for (path, kind, magic) in [
        ("/favicon.ico", "image/x-icon", &[0u8, 0, 1, 0][..]),
        ("/apple-touch-icon.png", "image/png", &b"\x89PNG"[..]),
    ] {
        let resp = app(temp_dir())
            .await
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
    let (s, v) = call(&app(temp_dir()).await, "GET", "/api/config", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["generate"], false);
    assert_eq!(v["save"], true);
    assert_eq!(v["auth"], "none");
    assert_eq!(v["googleClientId"], Value::Null);
    assert_eq!(v["models"][0]["id"], "test/model");
    // without sign-in, everyone is the local account, unmetered
    let (_, me) = call(&app(temp_dir()).await, "GET", "/api/me", None).await;
    assert_eq!(me["user"]["username"], "local");
    assert_eq!(me["user"]["local"], true);
    assert_eq!(me["user"]["ai"]["limit"], Value::Null);
}

#[tokio::test]
async fn beats_crud_round_trip() {
    let dir = temp_dir();
    let app = app(dir.clone()).await;

    let (s, v) = call(&app, "GET", "/api/beats", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["beats"], json!([]));
    assert_eq!(v["username"], "local");

    let (s, a) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"  First  ","beat":{"bpm":90,"pages":2,"synths":[{},{}]}})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(a["id"], "1");
    assert_eq!(a["name"], "First");
    assert_eq!(
        (a["bpm"].as_f64(), a["pages"].as_i64(), a["synths"].as_i64()),
        (Some(90.0), Some(2), Some(2))
    );
    assert!(dir.join("sequencer.db").exists());

    let (_, b) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"","beat":{"bpm":120}})),
    )
    .await;
    assert_eq!(b["name"], "Untitled beat");
    assert_eq!(b["id"], "2");

    // Update keeps createdAt, bumps updatedAt, and moves it to the top of the list.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let (s, u) = call(
        &app,
        "PUT",
        "/api/beats/1",
        Some(json!({"name":"Renamed","beat":{"bpm":95}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(u["createdAt"], a["createdAt"]);
    assert!(u["updatedAt"].as_i64() > a["updatedAt"].as_i64());
    assert_eq!(u["beat"]["bpm"], 95);

    // The list is summaries; one beat's JSON comes from its own URL.
    let (_, v) = call(&app, "GET", "/api/beats", None).await;
    let beats = v["beats"].as_array().unwrap();
    assert_eq!(beats.len(), 2);
    assert_eq!(beats[0]["name"], "Renamed");
    assert!(beats[0].get("beat").is_none());
    let (s, one) = call(&app, "GET", "/api/beats/1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(one["beat"]["bpm"], 95);

    // Updating a beat that isn't there doesn't create it.
    let (s, _) = call(&app, "PUT", "/api/beats/7", Some(json!({"beat":{}}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _) = call(&app, "DELETE", "/api/beats/1", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = call(&app, "DELETE", "/api/beats/1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Numbers are never reused, so an old link can't point at a different beat.
    let (_, c) = call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"Third","beat":{}})),
    )
    .await;
    assert_eq!(c["id"], "3");

    // A fresh server over the same dir sees the same beats.
    let (_, v) = call(&self::app(dir).await, "GET", "/api/beats", None).await;
    assert_eq!(v["beats"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn rejects_bad_ids_and_bodies() {
    let app = app(temp_dir()).await;
    for bad in ["..%2Fetc", "abc", "01", "0", "1.5"] {
        let (s, _) = call(
            &app,
            "PUT",
            &format!("/api/beats/{bad}"),
            Some(json!({"beat":{}})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
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
async fn shared_links_open_anyones_beat() {
    let app = app(temp_dir()).await;
    call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"Night <Drive>","beat":{"bpm":100}})),
    )
    .await;
    let (s, v) = call(&app, "GET", "/api/u/local/1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["username"], "local");
    assert_eq!(v["beat"]["bpm"], 100);
    let (s, _) = call(&app, "GET", "/api/u/local/2", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // The page itself, with a link preview naming the beat (escaped).
    let resp = send(&app, "GET", "/local/1", None, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(
        html.contains(r#"<meta property="og:title" content="Night &lt;Drive&gt; by local">"#),
        "no preview title"
    );
    for missing in ["/local/2", "/local/01", "/nobody/1", "/api/1"] {
        let resp = send(&app, "GET", missing, None, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{missing}");
    }
}

#[tokio::test]
async fn import_appends_in_order_and_keeps_dates() {
    let app = app(temp_dir()).await;
    call(
        &app,
        "POST",
        "/api/beats",
        Some(json!({"name":"Already here","beat":{}})),
    )
    .await;
    let (s, v) = call(
        &app,
        "POST",
        "/api/beats/import",
        Some(json!({"beats":[
            {"name":"Old","beat":{"bpm":80},"createdAt":1000,"updatedAt":2000},
            {"name":"Newer","beat":{"bpm":90},"createdAt":3000,"updatedAt":4000},
        ]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let got = v["beats"].as_array().unwrap();
    assert_eq!(
        (got[0]["id"].as_str(), got[1]["id"].as_str()),
        (Some("2"), Some("3"))
    );
    assert_eq!(
        (got[0]["createdAt"].as_i64(), got[0]["updatedAt"].as_i64()),
        (Some(1000), Some(2000))
    );
    let (_, v) = call(&app, "GET", "/api/beats", None).await;
    let names: Vec<&str> = v["beats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Already here", "Newer", "Old"]);
}

#[tokio::test]
async fn sign_in_pick_a_username_and_save() {
    let app = google_app(temp_dir(), None, "http://127.0.0.1:1").await;
    let (_, cfg) = call(&app, "GET", "/api/config", None).await;
    assert_eq!(cfg["auth"], "google");
    assert_eq!(cfg["googleClientId"], CLIENT_ID);

    // Signed out: no saving to the server, no AI.
    let (s, me) = call(&app, "GET", "/api/me", None).await;
    assert_eq!((s, &me["user"]), (StatusCode::OK, &Value::Null));
    let (s, v) = call(&app, "POST", "/api/beats", Some(json!({"beat":{}}))).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("signed_out"))
    );

    // First sign-in: an account with no username yet, which can't save.
    let (cookie, me) = sign_in(&app, "g-123").await;
    assert_eq!(me["user"]["username"], Value::Null);
    assert_eq!(me["user"]["email"], "g-123@example.com");
    let (s, v) = call_as(&app, &cookie, "GET", "/api/beats", None).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("needs_username"))
    );

    for (bad, why) in [
        ("ab", "3 to 20"),
        ("Akshay", "lowercase"),
        ("-ak", "Start"),
        ("api", "reserved"),
    ] {
        let (s, v) = call_as(
            &app,
            &cookie,
            "POST",
            "/api/auth/username",
            Some(json!({"username": bad})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
        assert!(v["message"].as_str().unwrap().contains(why), "{bad}: {v}");
    }
    let (_, v) = call(&app, "GET", "/api/usernames/akshay", None).await;
    assert_eq!(v["available"], true);
    let (s, me) = call_as(
        &app,
        &cookie,
        "POST",
        "/api/auth/username",
        Some(json!({"username":"akshay"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["user"]["username"], "akshay");
    assert_eq!(me["user"]["ai"]["limit"], 10);
    // It's permanent.
    let (s, _) = call_as(
        &app,
        &cookie,
        "POST",
        "/api/auth/username",
        Some(json!({"username":"other"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Someone else can't take it.
    let (c2, _) = sign_in(&app, "g-456").await;
    let (_, v) = call(&app, "GET", "/api/usernames/akshay", None).await;
    assert_eq!(v["available"], false);
    let (s, _) = call_as(
        &app,
        &c2,
        "POST",
        "/api/auth/username",
        Some(json!({"username":"akshay"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = call_as(
        &app,
        &c2,
        "POST",
        "/api/auth/username",
        Some(json!({"username":"friend"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // Each account numbers its own beats, and only sees its own.
    let (_, a) = call_as(
        &app,
        &cookie,
        "POST",
        "/api/beats",
        Some(json!({"name":"Mine","beat":{"bpm":91}})),
    )
    .await;
    let (_, b) = call_as(
        &app,
        &c2,
        "POST",
        "/api/beats",
        Some(json!({"name":"Theirs","beat":{}})),
    )
    .await;
    assert_eq!((a["id"].as_str(), b["id"].as_str()), (Some("1"), Some("1")));
    let (_, v) = call_as(&app, &c2, "GET", "/api/beats", None).await;
    assert_eq!(v["beats"].as_array().unwrap().len(), 1);
    assert_eq!(v["beats"][0]["name"], "Theirs");
    let (s, _) = call_as(&app, &c2, "DELETE", "/api/beats/2", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // ...but anyone can open a beat by its link.
    let (s, v) = call(&app, "GET", "/api/u/akshay/1", None).await;
    assert_eq!((s, v["name"].as_str()), (StatusCode::OK, Some("Mine")));

    // Signing in again finds the same account.
    let (c3, me) = sign_in(&app, "g-123").await;
    assert_eq!(me["user"]["username"], "akshay");
    // Signing out ends that session only.
    let resp = send(&app, "POST", "/api/auth/signout", None, Some(&c3)).await;
    assert!(
        resp.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let (_, me) = call_as(&app, &c3, "GET", "/api/me", None).await;
    assert_eq!(me["user"], Value::Null);
    let (_, me) = call_as(&app, &cookie, "GET", "/api/me", None).await;
    assert_eq!(me["user"]["username"], "akshay");
    // A made-up cookie is nobody.
    let (_, me) = call_as(&app, "sq_session=nope", "GET", "/api/me", None).await;
    assert_eq!(me["user"], Value::Null);
}

#[tokio::test]
async fn rejects_bad_google_tokens() {
    let app = google_app(temp_dir(), None, "http://127.0.0.1:1").await;
    let bad: Vec<(&str, String)> = vec![
        (
            "wrong audience",
            id_token("x", |c| c["aud"] = json!("someone-else")),
        ),
        (
            "wrong issuer",
            id_token("x", |c| c["iss"] = json!("https://evil.example")),
        ),
        (
            "expired",
            id_token("x", |c| c["exp"] = json!(now_s() - 3600)),
        ),
        ("no subject", id_token("x", |c| c["sub"] = json!(""))),
        ("tampered", {
            let t = id_token("x", |_| {});
            let p: Vec<&str> = t.split('.').collect();
            let forged = B64.encode(json!({"iss":"https://accounts.google.com","aud":CLIENT_ID,"sub":"admin","exp":now_s()+3600}).to_string());
            format!("{}.{forged}.{}", p[0], p[2])
        }),
        ("alg none", {
            let t = id_token("x", |_| {});
            let p: Vec<&str> = t.split('.').collect();
            format!(
                "{}.{}.",
                B64.encode(r#"{"alg":"none","kid":"test-kid"}"#),
                p[1]
            )
        }),
        ("garbage", "not.a.jwt".into()),
    ];
    for (why, tok) in bad {
        let (s, v) = call(
            &app,
            "POST",
            "/api/auth/google",
            Some(json!({"credential": tok})),
        )
        .await;
        assert_eq!(
            (s, v["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("bad_token")),
            "{why}"
        );
    }
}

#[tokio::test]
async fn ai_needs_an_account_and_counts_per_day() {
    let upstream = serve(Router::new().route(
        "/chat/completions",
        post(|body: String| async move {
            if body.contains("fail") {
                return (StatusCode::BAD_GATEWAY, [("content-type", "application/json")], "{}".to_string());
            }
            let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            (StatusCode::OK, [("content-type", "text/event-stream")], sse.to_string())
        }),
    ))
    .await;
    let dir = temp_dir();
    let app = google_app(dir.clone(), Some("k"), &upstream).await;
    let gen_ = json!({"model":"test/model","prompt":"a beat"});

    let (s, v) = call(&app, "POST", "/api/generate", Some(gen_.clone())).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("signed_out"))
    );

    let cookie = member(&app, "g-1", "maker").await;
    // a failed upstream call doesn't use up a generation
    let (s, _) = call_as(
        &app,
        &cookie,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"fail"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    for _ in 0..10 {
        let resp = send(
            &app,
            "POST",
            "/api/generate",
            Some(gen_.clone()),
            Some(&cookie),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        resp.into_body().collect().await.unwrap();
    }
    let (_, me) = call_as(&app, &cookie, "GET", "/api/me", None).await;
    assert_eq!(
        (
            me["user"]["ai"]["used"].as_i64(),
            me["user"]["ai"]["limit"].as_i64()
        ),
        (Some(10), Some(10))
    );
    let (s, v) = call_as(&app, &cookie, "POST", "/api/generate", Some(gen_.clone())).await;
    assert_eq!(
        (s, v["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("daily_limit"))
    );

    // `sequencer set-ai-limit` raises one account's limit.
    let db = crate::db::Db::open(&dir.join("sequencer.db")).unwrap();
    assert!(db.set_ai_limit("maker".into(), Some(12)).await.unwrap());
    let (_, me) = call_as(&app, &cookie, "GET", "/api/me", None).await;
    assert_eq!(me["user"]["ai"]["limit"], 12);
    let resp = send(&app, "POST", "/api/generate", Some(gen_), Some(&cookie)).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn rejects_oversize_body() {
    let big = "x".repeat(BODY_LIMIT + 1);
    let (s, v) = call(
        &app(temp_dir()).await,
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
        &app(temp_dir()).await,
        "POST",
        "/api/generate",
        Some(json!({"model":"test/model","prompt":"hi"})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["code"], "not_configured");

    let keyed = router(cfg(temp_dir(), Some("k"), "http://127.0.0.1:1")).await;
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
    let base = serve(mock).await;

    let app = router(cfg(temp_dir(), Some("k"), &base)).await;
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
    let base = serve(mock).await;

    let app = router(cfg(temp_dir(), Some("k"), &base)).await;
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
    let base = serve(mock).await;
    let app = router(cfg(temp_dir(), Some("k"), &base)).await;
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
    let keyed = router(cfg(temp_dir(), Some("k"), "http://127.0.0.1:1")).await;
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
    let base = serve(mock).await;
    let app = router(cfg(temp_dir(), Some("k"), &base)).await;
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
