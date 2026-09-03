use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

pub const MAX_EVENTS: usize = 50;
pub const MAX_SESSIONS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub device_name: Option<String>,
    pub created_at: u64,
    pub last_pushed_at: Option<u64>,
    pub fail_count: u32,
}

/// Privacy-safe snapshot of a completion event, kept for the status page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub event: String,
    pub ts: u64,
    pub session_name: Option<String>,
    pub hostname: Option<String>,
    pub cwd: Option<String>,
    pub duration_min: Option<u64>,
}

/// Persistent session record. Authoritative for the dashboard list; fed by the
/// extension runtime reports AND merged from disk scans of the OMP session
/// transcripts. Survives restarts (lives in store.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub hostname: Option<String>,
    /// Last known model, compact "provider/id" form.
    pub model: Option<String>,
    pub latest_user_prompt: Option<String>,
    pub latest_assistant_preview: Option<String>,
    /// "active" | "completed" | "error" — the terminal state when known.
    pub state: String,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    pub last_seen_at: u64,
}

impl SessionRecord {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            title: None,
            cwd: None,
            hostname: None,
            model: None,
            latest_user_prompt: None,
            latest_assistant_preview: None,
            state: "active".to_string(),
            started_at: None,
            ended_at: None,
            last_seen_at: now_unix(),
        }
    }
}

/// In-memory live state only; never persisted. Distinct from SessionRecord:
/// carries transient activity that must not become stale truth.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeRecord {
    pub session_id: String,
    pub activity: Option<String>, // "running" | "idle" | null
    pub activity_detail: Option<String>,
    pub last_seen_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreData {
    pub version: u32,
    pub subscriptions: Vec<Subscription>,
    pub events: Vec<StoredEvent>,
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
}

impl Default for StoreData {
    fn default() -> Self {
        Self {
            version: 1,
            subscriptions: Vec::new(),
            events: Vec::new(),
            sessions: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct Store {
    pub data: StoreData,
    /// Live runtime registry, keyed by session_id. Memory only by design:
    /// transient activity signals are never persisted as truth.
    pub runtime: HashMap<String, RuntimeRecord>,
    path: PathBuf,
}

impl Store {
    /// Missing file -> empty store. Corrupt file -> rename to `.bad` and start
    /// fresh (single-user tool; never block the server on a broken JSON file).
    pub fn load(path: PathBuf) -> Self {
        match fs::read(&path) {
            Err(_) => Store { data: StoreData::default(), path, runtime: HashMap::new() },
            Ok(bytes) => match serde_json::from_slice::<StoreData>(&bytes) {
                Ok(data) => Store { data, path, runtime: HashMap::new() },
                Err(e) => {
                    let bad = path.with_extension("json.bad");
                    let _ = fs::rename(&path, &bad);
                    tracing::warn!("store corrupt ({e}); moved to {}", bad.display());
                    Store { data: StoreData::default(), path, runtime: HashMap::new() }
                }
            },
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(p) = self.path.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.data)?)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn push_event(&mut self, ev: StoredEvent) {
        self.data.events.push(ev);
        if self.data.events.len() > MAX_EVENTS {
            let over = self.data.events.len() - MAX_EVENTS;
            self.data.events.drain(..over);
        }
    }

    /// Upsert a session record by id, preserving unknown fields from the
    /// existing record (merge semantics: incoming non-None wins).
    pub fn upsert_session(&mut self, incoming: SessionRecord) {
        if let Some(existing) = self.data.sessions.iter_mut().find(|s| s.session_id == incoming.session_id) {
            if incoming.title.is_some() {
                existing.title = incoming.title;
            }
            if incoming.cwd.is_some() {
                existing.cwd = incoming.cwd;
            }
            if incoming.hostname.is_some() {
                existing.hostname = incoming.hostname;
            }
            if incoming.model.is_some() {
                existing.model = incoming.model;
            }
            if incoming.latest_user_prompt.is_some() {
                existing.latest_user_prompt = incoming.latest_user_prompt;
            }
            if incoming.latest_assistant_preview.is_some() {
                existing.latest_assistant_preview = incoming.latest_assistant_preview;
            }
            if incoming.state != "active" || existing.state == "active" {
                existing.state = incoming.state;
            }
            if incoming.started_at.is_some() {
                existing.started_at = incoming.started_at;
            }
            if incoming.ended_at.is_some() {
                existing.ended_at = incoming.ended_at;
            }
            existing.last_seen_at = incoming.last_seen_at;
        } else {
            self.data.sessions.push(incoming);
        }
        if self.data.sessions.len() > MAX_SESSIONS {
            let over = self.data.sessions.len() - MAX_SESSIONS;
            self.data.sessions.drain(..over);
        }
    }

    pub fn get_session(&self, session_id: &str) -> Option<&SessionRecord> {
        self.data.sessions.iter().find(|s| s.session_id == session_id)
    }
}

pub fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}