//! Google sign-in and sessions.
//!
//! The browser signs in with Google Identity Services and POSTs the ID token it gets
//! (a JWT) to `/api/auth/google`. We check its RS256 signature against Google's published
//! keys, plus the issuer, audience (our client id) and expiry, then set an HttpOnly
//! session cookie. The cookie holds a random token; the database only keeps its SHA-256,
//! so a leaked database can't be replayed as sessions.
//!
//! Without `GOOGLE_CLIENT_ID` there's no sign-in at all: every request acts as one
//! built-in `local` account, which suits a server on your own network.

use crate::AppState;
use crate::db::{DbError, User, now_ms};
use crate::handlers::err;
use axum::{
    Json,
    extract::{FromRequestParts, Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{digest, rand::SecureRandom, signature};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

pub const COOKIE: &str = "sq_session";
const SESSION_MS: i64 = 60 * 86_400_000;
pub const DEFAULT_CERTS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";
const ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];
/// Refetch Google's keys at most this often when a token names a key we don't know.
const REFETCH_GAP: Duration = Duration::from_secs(60);
/// Google rotates keys every few weeks and publishes new ones well ahead.
const KEYS_TTL: Duration = Duration::from_secs(3600);
/// Clock slack when checking `exp` and `iat`.
const LEEWAY_S: i64 = 60;

/// Path segments that can't be usernames, since `/<username>/<n>` shares the URL space.
const RESERVED: &[&str] = &[
    "about",
    "account",
    "admin",
    "api",
    "app",
    "assets",
    "auth",
    "beat",
    "beats",
    "explore",
    "healthz",
    "help",
    "home",
    "local",
    "login",
    "logout",
    "me",
    "new",
    "null",
    "privacy",
    "root",
    "settings",
    "share",
    "signin",
    "signout",
    "signup",
    "static",
    "support",
    "terms",
    "undefined",
    "user",
    "users",
    "www",
];

/// 3 to 20 of `a-z 0-9 _ -`, starting with a letter or digit.
pub fn username_problem(u: &str) -> Option<&'static str> {
    if u.len() < 3 || u.len() > 20 {
        return Some("Use 3 to 20 characters.");
    }
    if !u
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Some("Use lowercase letters, numbers, - and _ only.");
    }
    if !u.as_bytes()[0].is_ascii_alphanumeric() {
        return Some("Start with a letter or a number.");
    }
    if RESERVED.contains(&u) {
        return Some("That one's reserved.");
    }
    None
}

// ---- Google ID tokens ----

#[derive(Default)]
struct Keys {
    fetched: Option<Instant>,
    rsa: HashMap<String, (Vec<u8>, Vec<u8>)>,
}

pub struct Google {
    pub client_id: String,
    certs_url: String,
    keys: RwLock<Keys>,
}

#[derive(Debug, PartialEq)]
pub struct Claims {
    pub sub: String,
    pub email: String,
}

impl Google {
    pub fn new(client_id: String, certs_url: String) -> Self {
        Google {
            client_id,
            certs_url,
            keys: RwLock::new(Keys::default()),
        }
    }

    async fn fetch_keys(&self, client: &reqwest::Client) {
        let mut k = self.keys.write().await;
        if k.fetched.is_some_and(|t| t.elapsed() < REFETCH_GAP) {
            return; // another request just did
        }
        k.fetched = Some(Instant::now());
        let got = async {
            let v: Value = client
                .get(&self.certs_url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            let mut rsa = HashMap::new();
            for key in v["keys"].as_array()? {
                if key["kty"] != "RSA" {
                    continue;
                }
                let (Some(kid), Some(n), Some(e)) =
                    (key["kid"].as_str(), key["n"].as_str(), key["e"].as_str())
                else {
                    continue;
                };
                if let (Ok(n), Ok(e)) = (B64.decode(n), B64.decode(e)) {
                    rsa.insert(kid.to_string(), (n, e));
                }
            }
            Some(rsa)
        }
        .await;
        match got {
            Some(rsa) if !rsa.is_empty() => k.rsa = rsa,
            _ => eprintln!("sequencer: couldn't fetch Google's sign-in keys"),
        }
    }

    async fn key(&self, client: &reqwest::Client, kid: &str) -> Option<(Vec<u8>, Vec<u8>)> {
        {
            let k = self.keys.read().await;
            let fresh = k.fetched.is_some_and(|t| t.elapsed() < KEYS_TTL);
            if let Some(found) = k.rsa.get(kid)
                && fresh
            {
                return Some(found.clone());
            }
        }
        self.fetch_keys(client).await;
        self.keys.read().await.rsa.get(kid).cloned()
    }

    /// Check a Google ID token and return who it's for.
    pub async fn verify(
        &self,
        client: &reqwest::Client,
        token: &str,
    ) -> Result<Claims, &'static str> {
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err("not a JWT");
        };
        let header: Value = B64
            .decode(h)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("bad header")?;
        if header["alg"] != "RS256" {
            return Err("unexpected algorithm");
        }
        let kid = header["kid"].as_str().ok_or("no key id")?;
        let sig = B64.decode(s).map_err(|_| "bad signature encoding")?;
        let (n, e) = self.key(client, kid).await.ok_or("unknown signing key")?;
        signature::RsaPublicKeyComponents { n: &n, e: &e }
            .verify(
                &signature::RSA_PKCS1_2048_8192_SHA256,
                &token.as_bytes()[..h.len() + 1 + p.len()],
                &sig,
            )
            .map_err(|_| "bad signature")?;
        let c: Value = B64
            .decode(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or("bad payload")?;
        if !ISSUERS.iter().any(|i| c["iss"] == *i) {
            return Err("wrong issuer");
        }
        let aud_ok = match &c["aud"] {
            Value::String(a) => *a == self.client_id,
            Value::Array(a) => a.iter().any(|x| *x == *self.client_id),
            _ => false,
        };
        if !aud_ok {
            return Err("wrong audience");
        }
        let now = now_ms() / 1000;
        if c["exp"].as_i64().is_none_or(|exp| exp + LEEWAY_S < now) {
            return Err("expired");
        }
        if c["iat"].as_i64().is_some_and(|iat| iat > now + LEEWAY_S) {
            return Err("issued in the future");
        }
        let sub = c["sub"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("no subject")?;
        Ok(Claims {
            sub: sub.to_string(),
            email: c["email"].as_str().unwrap_or("").to_string(),
        })
    }
}

// ---- sessions ----

fn hash(token: &str) -> String {
    digest::digest(&digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn new_token() -> String {
    let mut b = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .expect("system randomness");
    B64.encode(b)
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

fn set_cookie(value: &str, max_age_s: i64) -> (header::HeaderName, String) {
    (
        header::SET_COOKIE,
        format!("{COOKIE}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_s}"),
    )
}

/// Who's asking: the signed-in account, or nobody. On a server without sign-in it's
/// always the built-in `local` account.
pub struct Viewer(pub Option<User>);

impl FromRequestParts<AppState> for Viewer {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Infallible> {
        if state.google.is_none() {
            return Ok(Viewer(state.local_user.clone()));
        }
        let Some(tok) = cookie_token(&parts.headers) else {
            return Ok(Viewer(None));
        };
        Ok(Viewer(
            state.db.session_user(hash(&tok)).await.ok().flatten(),
        ))
    }
}

impl Viewer {
    /// The account, if it's signed in and has finished signing up (picked a username).
    // the error is the response itself, returned straight from the handler
    #[allow(clippy::result_large_err)]
    pub fn member(self) -> Result<(User, String), Response> {
        match self.0 {
            None => Err(err(
                StatusCode::UNAUTHORIZED,
                "signed_out",
                "Sign in to do that.",
            )),
            Some(User { username: None, .. }) => Err(err(
                StatusCode::FORBIDDEN,
                "needs_username",
                "Pick a username first.",
            )),
            Some(u) => {
                let name = u.username.clone().unwrap_or_default();
                Ok((u, name))
            }
        }
    }
}

pub fn db_failure(e: DbError) -> Response {
    match e {
        DbError::BadRequest(m) => err(StatusCode::BAD_REQUEST, "invalid_argument", m),
        DbError::NotFound => err(StatusCode::NOT_FOUND, "not_found", "no such beat"),
        DbError::Full => err(
            StatusCode::INSUFFICIENT_STORAGE,
            "quota_exceeded",
            "too many saved beats",
        ),
        DbError::Sql(e) => {
            eprintln!("sequencer: database error: {e}");
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                "couldn't read or write the database",
            )
        }
    }
}

/// The account as the browser sees it. `null` when signed out.
async fn me_json(state: &AppState, user: Option<&User>) -> Value {
    let Some(u) = user else {
        return json!({ "user": null });
    };
    let used = state.db.ai_used(u.id).await.unwrap_or(0);
    let limit = state.ai_limit(u);
    json!({ "user": {
        "username": u.username,
        "email": u.email,
        "local": state.google.is_none(),
        "ai": { "used": used, "limit": limit, "resetsAt": (crate::db::today() + 1) * 86_400_000 },
    }})
}

pub async fn me(State(state): State<AppState>, viewer: Viewer) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(me_json(&state, viewer.0.as_ref()).await),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct GoogleBody {
    credential: String,
}

pub async fn google_sign_in(
    State(state): State<AppState>,
    body: Result<Json<GoogleBody>, JsonRejection>,
) -> Response {
    let Some(google) = state.google.clone() else {
        return err(
            StatusCode::NOT_FOUND,
            "not_configured",
            "Sign-in is off on this server.",
        );
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return err(r.status(), "invalid_argument", &r.body_text()),
    };
    let claims = match google.verify(&state.client, &b.credential).await {
        Ok(c) => c,
        Err(why) => {
            eprintln!("sequencer: rejected a Google sign-in: {why}");
            return err(
                StatusCode::UNAUTHORIZED,
                "bad_token",
                "Google sign-in didn't check out. Try again.",
            );
        }
    };
    let user = match state.db.google_user(claims.sub, claims.email).await {
        Ok(u) => u,
        Err(e) => return db_failure(e),
    };
    let tok = new_token();
    if let Err(e) = state
        .db
        .create_session(hash(&tok), user.id, now_ms() + SESSION_MS)
        .await
    {
        return db_failure(e);
    }
    (
        [set_cookie(&tok, SESSION_MS / 1000)],
        Json(me_json(&state, Some(&user)).await),
    )
        .into_response()
}

pub async fn sign_out(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = cookie_token(&headers) {
        let _ = state.db.delete_session(hash(&tok)).await;
    }
    ([set_cookie("", 0)], StatusCode::NO_CONTENT).into_response()
}

pub async fn username_check(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if let Some(why) = username_problem(&name) {
        return Json(json!({ "available": false, "reason": why })).into_response();
    }
    match state.db.username_taken(name).await {
        Ok(taken) => Json(json!({
            "available": !taken,
            "reason": if taken { Some("That one's taken.") } else { None },
        }))
        .into_response(),
        Err(e) => db_failure(e),
    }
}

#[derive(Deserialize)]
pub struct UsernameBody {
    username: String,
}

/// Pick a username, once, right after the first sign-in. It's part of every beat's
/// address, so it can't change later.
pub async fn set_username(
    State(state): State<AppState>,
    viewer: Viewer,
    body: Result<Json<UsernameBody>, JsonRejection>,
) -> Response {
    let Some(user) = viewer.0 else {
        return err(StatusCode::UNAUTHORIZED, "signed_out", "Sign in first.");
    };
    let Json(b) = match body {
        Ok(b) => b,
        Err(r) => return err(r.status(), "invalid_argument", &r.body_text()),
    };
    let name = b.username.trim().to_string();
    if let Some(why) = username_problem(&name) {
        return err(StatusCode::BAD_REQUEST, "invalid_username", why);
    }
    match state.db.set_username(user.id, name.clone()).await {
        Ok(true) => {
            let user = User {
                username: Some(name),
                ..user
            };
            Json(me_json(&state, Some(&user)).await).into_response()
        }
        Ok(false) => err(StatusCode::CONFLICT, "taken", "That one's taken."),
        Err(e) => db_failure(e),
    }
}
