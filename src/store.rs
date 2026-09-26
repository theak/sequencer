//! Saved beats, stored as one JSON file per beat:
//! `$DATA_DIR/<owner>/beats/<id>.json`.
//!
//! No database: a directory of small files is trivial to back up, works on a Docker
//! volume, and works unchanged on Cloud Run with a Cloud Storage bucket mounted at
//! `DATA_DIR`. Every call is scoped to an `Owner`, so adding sign-in later only changes
//! how the owner is resolved, not how beats are stored.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub const MAX_BEATS: usize = 1000;
pub const LIST_LIMIT: usize = 200;
pub const MAX_NAME: usize = 60;

/// Whose beats a request reads and writes.
///
/// There is no sign-in yet, so everyone shares the `local` owner (a self-hosted
/// instance is one person's or one household's). When Google sign-in lands, this
/// extractor resolves the owner from the session cookie instead.
#[derive(Clone, Debug, PartialEq)]
pub struct Owner(pub String);

impl<S: Send + Sync> FromRequestParts<S> for Owner {
    type Rejection = Infallible;
    async fn from_request_parts(_parts: &mut Parts, _state: &S) -> Result<Self, Infallible> {
        Ok(Owner("local".into()))
    }
}

#[derive(Debug)]
pub enum StoreError {
    BadRequest(&'static str),
    Full,
    Io(std::io::Error),
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        StoreError::Io(e)
    }
}

/// Ids and owners become path components, so both are restricted to a safe alphabet.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

fn valid_owner(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct Store {
    root: PathBuf,
    // Serializes writes so the beat-count check and the write can't race.
    write_lock: Mutex<()>,
    seq: AtomicU32,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Store {
            root,
            write_lock: Mutex::new(()),
            seq: AtomicU32::new(0),
        }
    }

    fn dir(&self, owner: &Owner) -> Result<PathBuf, StoreError> {
        if !valid_owner(&owner.0) {
            return Err(StoreError::BadRequest("bad owner"));
        }
        Ok(self.root.join(&owner.0).join("beats"))
    }

    /// Time-ordered, collision-free within this process: millis + a rolling counter.
    fn new_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) & 0xffff;
        format!("{:x}{:04x}", now_ms(), n)
    }

    async fn read_all(&self, owner: &Owner) -> Result<Vec<Value>, StoreError> {
        let dir = self.dir(owner)?;
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![];
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name();
            let Some(id) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            // Skip unreadable or corrupt files rather than failing the whole list.
            if let Ok(bytes) = tokio::fs::read(ent.path()).await
                && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                && v.is_object()
            {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// Newest first, capped at `LIST_LIMIT`.
    pub async fn list(&self, owner: &Owner) -> Result<Vec<Value>, StoreError> {
        let mut all = self.read_all(owner).await?;
        all.sort_by_key(|v| std::cmp::Reverse(v["updatedAt"].as_u64().unwrap_or(0)));
        all.truncate(LIST_LIMIT);
        Ok(all)
    }

    async fn read(&self, owner: &Owner, id: &str) -> Option<Value> {
        let path = self.dir(owner).ok()?.join(format!("{id}.json"));
        let bytes = tokio::fs::read(path).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn write(&self, owner: &Owner, id: &str, doc: &Value) -> Result<(), StoreError> {
        let dir = self.dir(owner)?;
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = dir.join(format!("{id}.json.tmp"));
        tokio::fs::write(&tmp, serde_json::to_vec(doc).expect("serializable")).await?;
        tokio::fs::rename(&tmp, dir.join(format!("{id}.json"))).await?;
        Ok(())
    }

    /// Create (`id: None`) or upsert a beat. The server owns `id` and the timestamps;
    /// `createdAt` is kept across updates.
    pub async fn save(
        &self,
        owner: &Owner,
        id: Option<&str>,
        name: &str,
        beat: Value,
    ) -> Result<Value, StoreError> {
        if !beat.is_object() {
            return Err(StoreError::BadRequest("beat must be an object"));
        }
        let name: String = match name.trim() {
            "" => "Untitled beat".into(),
            n => n.chars().take(MAX_NAME).collect(),
        };
        if let Some(id) = id
            && !valid_id(id)
        {
            return Err(StoreError::BadRequest("bad id"));
        }

        let _guard = self.write_lock.lock().await;
        let existing = match id {
            Some(id) => self.read(owner, id).await,
            None => None,
        };
        if existing.is_none() && self.read_all(owner).await?.len() >= MAX_BEATS {
            return Err(StoreError::Full);
        }
        let id = id.map(str::to_string).unwrap_or_else(|| self.new_id());
        let now = now_ms();
        let created = existing
            .as_ref()
            .and_then(|e| e["createdAt"].as_u64())
            .unwrap_or(now);
        let doc =
            json!({ "id": id, "name": name, "beat": beat, "createdAt": created, "updatedAt": now });
        self.write(owner, &id, &doc).await?;
        Ok(doc)
    }

    /// Returns whether a beat was actually removed.
    pub async fn delete(&self, owner: &Owner, id: &str) -> Result<bool, StoreError> {
        if !valid_id(id) {
            return Err(StoreError::BadRequest("bad id"));
        }
        let path = self.dir(owner)?.join(format!("{id}.json"));
        let _guard = self.write_lock.lock().await;
        match tokio::fs::remove_file(path).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
