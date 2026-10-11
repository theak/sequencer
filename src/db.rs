//! Everything the server keeps lives in one SQLite file, `$DATA_DIR/sequencer.db`:
//! accounts, sign-in sessions, saved beats and the daily AI usage counts.
//!
//! rusqlite is synchronous, so every query runs on tokio's blocking pool behind one
//! mutex-guarded connection. That's plenty for this app, and it means writes never race.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_BEATS: i64 = 1000;
pub const MAX_NAME: usize = 60;
/// One beat's JSON (a 4-page beat with several synths is ~10 kB).
pub const MAX_BEAT_BYTES: usize = 256 * 1024;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users(
  id INTEGER PRIMARY KEY,
  google_sub TEXT UNIQUE,
  email TEXT NOT NULL DEFAULT '',
  username TEXT UNIQUE,
  next_beat INTEGER NOT NULL DEFAULT 1,
  ai_limit INTEGER,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions(
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  expires_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS beats(
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  num INTEGER NOT NULL,
  name TEXT NOT NULL,
  beat TEXT NOT NULL,
  bpm REAL NOT NULL DEFAULT 0,
  pages INTEGER NOT NULL DEFAULT 1,
  synths INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY(user_id, num)
);
CREATE INDEX IF NOT EXISTS beats_by_updated ON beats(user_id, updated_at DESC);
CREATE TABLE IF NOT EXISTS ai_usage(
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  day INTEGER NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY(user_id, day)
);
PRAGMA user_version = 1;
";

#[derive(Debug)]
pub enum DbError {
    BadRequest(&'static str),
    NotFound,
    Full,
    Sql(rusqlite::Error),
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sql(e)
    }
}

pub type DbResult<T> = Result<T, DbError>;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Days since the epoch in UTC. AI usage resets when this ticks over.
pub fn today() -> i64 {
    now_ms() / 86_400_000
}

#[derive(Clone, Debug, PartialEq)]
pub struct User {
    pub id: i64,
    /// `None` until the person picks one, right after their first sign-in.
    pub username: Option<String>,
    pub email: String,
    /// A per-account AI limit set with `sequencer set-ai-limit`, else the server default.
    pub ai_limit: Option<i64>,
}

fn user_row(r: &rusqlite::Row) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        ai_limit: r.get(3)?,
    })
}
const USER_COLS: &str = "id, username, email, ai_limit";

/// The summary columns of a beat as the API returns it (`beat` is added when asked for).
fn beat_row(r: &rusqlite::Row, with_beat: bool) -> rusqlite::Result<Value> {
    let num: i64 = r.get(0)?;
    let mut v = json!({
        "id": num.to_string(),
        "name": r.get::<_, String>(1)?,
        "bpm": r.get::<_, f64>(2)?,
        "pages": r.get::<_, i64>(3)?,
        "synths": r.get::<_, i64>(4)?,
        "createdAt": r.get::<_, i64>(5)?,
        "updatedAt": r.get::<_, i64>(6)?,
    });
    if with_beat {
        let text: String = r.get(7)?;
        v["beat"] = serde_json::from_str(&text).unwrap_or(Value::Null);
    }
    Ok(v)
}
const BEAT_COLS: &str = "num, name, bpm, pages, synths, created_at, updated_at, beat";

/// A beat to store: a cleaned-up name, the JSON text and the bits the list shows.
pub struct NewBeat {
    name: String,
    text: String,
    bpm: f64,
    pages: i64,
    synths: i64,
    created_at: Option<i64>,
    updated_at: Option<i64>,
}

impl NewBeat {
    /// `times` (created, updated) keeps the dates of a beat moved in from a browser.
    pub fn new(name: &str, beat: &Value, times: (Option<i64>, Option<i64>)) -> DbResult<NewBeat> {
        if !beat.is_object() {
            return Err(DbError::BadRequest("beat must be an object"));
        }
        let text = beat.to_string();
        if text.len() > MAX_BEAT_BYTES {
            return Err(DbError::BadRequest("beat is too big"));
        }
        let name: String = match name.trim() {
            "" => "Untitled beat".into(),
            n => n.chars().take(MAX_NAME).collect(),
        };
        let now = now_ms();
        Ok(NewBeat {
            name,
            text,
            bpm: beat["bpm"].as_f64().unwrap_or(0.0),
            pages: beat["pages"].as_i64().unwrap_or(1),
            synths: beat["synths"].as_array().map_or(0, |a| a.len() as i64),
            created_at: times.0.filter(|&t| t > 0 && t <= now),
            updated_at: times.1.filter(|&t| t > 0 && t <= now),
        })
    }
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` against the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> DbResult<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut c)
        })
        .await
        .expect("db task panicked")
    }

    // ---- accounts ----

    /// The single account a server without sign-in saves everything under.
    pub async fn local_user(&self) -> DbResult<User> {
        self.call(|c| {
            c.execute(
                "INSERT INTO users(username, created_at) VALUES('local', ?1) ON CONFLICT(username) DO NOTHING",
                params![now_ms()],
            )?;
            Ok(c.query_row(
                &format!("SELECT {USER_COLS} FROM users WHERE username = 'local'"),
                [],
                user_row,
            )?)
        })
        .await
    }

    /// Find or create the account for a Google sign-in, keeping its email current.
    pub async fn google_user(&self, sub: String, email: String) -> DbResult<User> {
        self.call(move |c| {
            Ok(c.query_row(
                &format!(
                    "INSERT INTO users(google_sub, email, created_at) VALUES(?1, ?2, ?3)
                     ON CONFLICT(google_sub) DO UPDATE SET email = excluded.email
                     RETURNING {USER_COLS}"
                ),
                params![sub, email, now_ms()],
                user_row,
            )?)
        })
        .await
    }

    pub async fn username_taken(&self, name: String) -> DbResult<bool> {
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE username = ?1)",
                [name],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Set a username once. `Ok(false)` if someone already has it.
    pub async fn set_username(&self, user_id: i64, name: String) -> DbResult<bool> {
        self.call(move |c| {
            let n = c.execute(
                "UPDATE users SET username = ?2 WHERE id = ?1 AND username IS NULL
                 AND NOT EXISTS(SELECT 1 FROM users WHERE username = ?2)",
                params![user_id, name],
            )?;
            if n == 0 {
                let has: Option<String> =
                    c.query_row("SELECT username FROM users WHERE id = ?1", [user_id], |r| {
                        r.get(0)
                    })?;
                if has.is_some() {
                    return Err(DbError::BadRequest("you already have a username"));
                }
            }
            Ok(n == 1)
        })
        .await
    }

    /// `None` resets the account to the server's default.
    pub async fn set_ai_limit(&self, username: String, limit: Option<i64>) -> DbResult<bool> {
        self.call(move |c| {
            Ok(c.execute(
                "UPDATE users SET ai_limit = ?2 WHERE username = ?1",
                params![username, limit],
            )? == 1)
        })
        .await
    }

    // ---- sessions ----

    pub async fn create_session(
        &self,
        token_hash: String,
        user_id: i64,
        expires_at: i64,
    ) -> DbResult<()> {
        self.call(move |c| {
            c.execute(
                "DELETE FROM sessions WHERE expires_at < ?1",
                params![now_ms()],
            )?;
            c.execute(
                "INSERT INTO sessions(token_hash, user_id, expires_at) VALUES(?1, ?2, ?3)",
                params![token_hash, user_id, expires_at],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn session_user(&self, token_hash: String) -> DbResult<Option<User>> {
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT u.id, u.username, u.email, u.ai_limit FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                params![token_hash, now_ms()],
                user_row,
            )
            .optional()?)
        })
        .await
    }

    pub async fn delete_session(&self, token_hash: String) -> DbResult<()> {
        self.call(move |c| {
            c.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
            Ok(())
        })
        .await
    }

    // ---- beats ----

    /// Summaries (no beat JSON), newest first.
    pub async fn list_beats(&self, user_id: i64) -> DbResult<Vec<Value>> {
        self.call(move |c| {
            let mut st = c.prepare(&format!(
                "SELECT {BEAT_COLS} FROM beats WHERE user_id = ?1 ORDER BY updated_at DESC, num DESC LIMIT ?2"
            ))?;
            let rows = st.query_map(params![user_id, MAX_BEATS], |r| beat_row(r, false))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn get_beat(&self, user_id: i64, num: i64) -> DbResult<Option<Value>> {
        self.call(move |c| {
            Ok(c.query_row(
                &format!("SELECT {BEAT_COLS} FROM beats WHERE user_id = ?1 AND num = ?2"),
                params![user_id, num],
                |r| beat_row(r, true),
            )
            .optional()?)
        })
        .await
    }

    /// Anyone's beat by its public address, `/<username>/<num>`.
    pub async fn public_beat(&self, username: String, num: i64) -> DbResult<Option<Value>> {
        self.call(move |c| {
            Ok(c.query_row(
                &format!(
                    "SELECT {} FROM beats b JOIN users u ON u.id = b.user_id WHERE u.username = ?1 AND b.num = ?2",
                    BEAT_COLS.split(", ").map(|c| format!("b.{c}")).collect::<Vec<_>>().join(", ")
                ),
                params![username, num],
                |r| beat_row(r, true),
            )
            .optional()?)
        })
        .await
    }

    /// Add beats in order, each taking the account's next number (numbers are never
    /// reused, so an old link never points at a different beat).
    pub async fn create_beats(&self, user_id: i64, beats: Vec<NewBeat>) -> DbResult<Vec<Value>> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let have: i64 = tx.query_row(
                "SELECT COUNT(*) FROM beats WHERE user_id = ?1",
                [user_id],
                |r| r.get(0),
            )?;
            if have + beats.len() as i64 > MAX_BEATS {
                return Err(DbError::Full);
            }
            let mut out = vec![];
            for b in beats {
                out.push(insert_beat(&tx, user_id, b)?);
            }
            tx.commit()?;
            Ok(out)
        })
        .await
    }

    pub async fn update_beat(&self, user_id: i64, num: i64, b: NewBeat) -> DbResult<Value> {
        self.call(move |c| {
            let n = c.execute(
                "UPDATE beats SET name = ?3, beat = ?4, bpm = ?5, pages = ?6, synths = ?7, updated_at = ?8
                 WHERE user_id = ?1 AND num = ?2",
                params![user_id, num, b.name, b.text, b.bpm, b.pages, b.synths, now_ms()],
            )?;
            if n == 0 {
                return Err(DbError::NotFound);
            }
            Ok(c.query_row(
                &format!("SELECT {BEAT_COLS} FROM beats WHERE user_id = ?1 AND num = ?2"),
                params![user_id, num],
                |r| beat_row(r, true),
            )?)
        })
        .await
    }

    pub async fn delete_beat(&self, user_id: i64, num: i64) -> DbResult<bool> {
        self.call(move |c| {
            Ok(c.execute(
                "DELETE FROM beats WHERE user_id = ?1 AND num = ?2",
                params![user_id, num],
            )? == 1)
        })
        .await
    }

    // ---- AI usage ----

    pub async fn ai_used(&self, user_id: i64) -> DbResult<i64> {
        self.call(move |c| {
            Ok(c.query_row(
                "SELECT COALESCE((SELECT count FROM ai_usage WHERE user_id = ?1 AND day = ?2), 0)",
                params![user_id, today()],
                |r| r.get(0),
            )?)
        })
        .await
    }

    /// Count one generation against today's limit. `Ok(None)` if the limit is used up,
    /// otherwise how many have been used today, this one included.
    pub async fn ai_take(&self, user_id: i64, limit: i64) -> DbResult<Option<i64>> {
        self.call(move |c| {
            let n = c.query_row(
                "INSERT INTO ai_usage(user_id, day, count) SELECT ?1, ?2, 1 WHERE ?3 > 0
                 ON CONFLICT(user_id, day) DO UPDATE SET count = count + 1 WHERE count < ?3
                 RETURNING count",
                params![user_id, today(), limit],
                |r| r.get(0),
            );
            match n {
                Ok(n) => Ok(Some(n)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Give back a generation that failed before the model wrote anything.
    pub async fn ai_refund(&self, user_id: i64) -> DbResult<()> {
        self.call(move |c| {
            c.execute(
                "UPDATE ai_usage SET count = count - 1 WHERE user_id = ?1 AND day = ?2 AND count > 0",
                params![user_id, today()],
            )?;
            Ok(())
        })
        .await
    }
}

fn insert_beat(tx: &Transaction, user_id: i64, b: NewBeat) -> DbResult<Value> {
    let num: i64 = tx.query_row(
        "UPDATE users SET next_beat = next_beat + 1 WHERE id = ?1 RETURNING next_beat - 1",
        [user_id],
        |r| r.get(0),
    )?;
    let now = now_ms();
    tx.execute(
        "INSERT INTO beats(user_id, num, name, beat, bpm, pages, synths, created_at, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            user_id,
            num,
            b.name,
            b.text,
            b.bpm,
            b.pages,
            b.synths,
            b.created_at.or(b.updated_at).unwrap_or(now),
            b.updated_at.unwrap_or(now)
        ],
    )?;
    Ok(tx.query_row(
        &format!("SELECT {BEAT_COLS} FROM beats WHERE user_id = ?1 AND num = ?2"),
        params![user_id, num],
        |r| beat_row(r, true),
    )?)
}
