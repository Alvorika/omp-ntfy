use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const MAX_EVENTS: usize = 50;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreData {
    pub version: u32,
    pub subscriptions: Vec<Subscription>,
    pub events: Vec<StoredEvent>,
}

impl Default for StoreData {
    fn default() -> Self {
        Self { version: 1, subscriptions: Vec::new(), events: Vec::new() }
    }
}

#[derive(Debug)]
pub struct Store {
    pub data: StoreData,
    path: PathBuf,
}

impl Store {
    /// Missing file -> empty store. Corrupt file -> rename to `.bad` and start
    /// fresh (single-user tool; never block the server on a broken JSON file).
    pub fn load(path: PathBuf) -> Self {
        match fs::read(&path) {
            Err(_) => Store { data: StoreData::default(), path },
            Ok(bytes) => match serde_json::from_slice::<StoreData>(&bytes) {
                Ok(data) => Store { data, path },
                Err(e) => {
                    let bad = path.with_extension("json.bad");
                    let _ = fs::rename(&path, &bad);
                    tracing::warn!("store corrupt ({e}); moved to {}", bad.display());
                    Store { data: StoreData::default(), path }
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
}

pub fn now_unix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}