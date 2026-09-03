use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_BIND: &str = "127.0.0.1:8090";
// Apple validates the VAPID subject strictly: it must be a well-formed
// mailto: or https: URI (e.g. "mailto:you@example.com"); bare addresses or
// non-FQDN hosts like "localhost" are rejected with 403 BadJwtToken.
// Set OMP_NTFY_SUBJECT to your real contact address in production.
pub const DEFAULT_SUBJECT: &str = "mailto:omp-ntfy@example.com";

/// Resolved XDG paths for config (secrets, VAPID keys) and state (subscriptions).
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env(config_flag: Option<&PathBuf>, data_flag: Option<&PathBuf>) -> Self {
        let config_dir = config_flag
            .cloned()
            .or_else(|| env_dir("OMP_NTFY_CONFIG_DIR"))
            .unwrap_or_else(|| xdg_default("XDG_CONFIG_HOME", ".config").join("omp-ntfy"));
        let data_dir = data_flag
            .cloned()
            .or_else(|| env_dir("OMP_NTFY_DATA_DIR"))
            .unwrap_or_else(|| xdg_default("XDG_STATE_HOME", ".local/state").join("omp-ntfy"));
        Self { config_dir, data_dir }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn store_file(&self) -> PathBuf {
        self.data_dir.join("store.json")
    }

    pub fn client_file(&self) -> PathBuf {
        self.config_dir.join("client.json")
    }

    pub fn settings_file(&self) -> PathBuf {
        // Settings carry no secrets and live in the writable state dir: the
        // systemd unit hardens the config dir read-only (ProtectHome).
        self.data_dir.join("settings.json")
    }
}

fn env_dir(var: &str) -> Option<PathBuf> {
    env::var(var).ok().filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn xdg_default(var: &str, fallback: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".into());
    env_dir(var).unwrap_or_else(|| PathBuf::from(home).join(fallback))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bind: String,
    pub token: String,
    /// url-safe base64 of the 32-byte P-256 private key (VAPID).
    pub vapid_private_key: String,
    /// VAPID subject, e.g. "mailto:you@example.com".
    pub vapid_subject: String,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let cfg: Self = serde_json::from_str(&raw)
            .with_context(|| format!("invalid config {}", path.display()))?;
        Ok(cfg.with_env_overrides())
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn generate(token: Option<String>, subject: Option<String>, bind: Option<String>) -> anyhow::Result<Self> {
        let (priv_b64, _pub) = crate::push::generate_vapid_keypair()?;
        Ok(Config {
            bind: bind
                .or_else(|| env::var("OMP_NTFY_BIND").ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| DEFAULT_BIND.into()),
            token: token
                .or_else(|| env::var("OMP_NTFY_TOKEN").ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(random_token),
            vapid_private_key: priv_b64,
            vapid_subject: subject
                .or_else(|| env::var("OMP_NTFY_SUBJECT").ok().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| DEFAULT_SUBJECT.into()),
        })
    }

    /// Environment overrides win over the config file, so native and Docker
    /// deployments share one configuration surface.
    pub fn with_env_overrides(mut self) -> Self {
        for (var, field) in [
            ("OMP_NTFY_BIND", &mut self.bind),
            ("OMP_NTFY_TOKEN", &mut self.token),
            ("OMP_NTFY_SUBJECT", &mut self.vapid_subject),
        ] {
            if let Ok(v) = env::var(var) {
                if !v.is_empty() {
                    *field = v;
                }
            }
        }
        self
    }

    pub fn vapid_public_key(&self) -> anyhow::Result<String> {
        crate::push::vapid_public_key(&self.vapid_private_key)
    }
}

fn random_token() -> String {
    use rand_core::TryRngCore;
    let mut b = [0u8; 32];
    rand_core::OsRng.try_fill_bytes(&mut b).expect("os rng");
    b.iter().map(|x| format!("{x:02x}")).collect()
}