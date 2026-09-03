use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const SETTINGS_VERSION: u32 = 1;
pub const MAX_PREVIEW_LENGTH: u64 = 1200;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub notifications: NotificationSettings,
    pub dashboard: DashboardSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            notifications: NotificationSettings::default(),
            dashboard: DashboardSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    pub enabled: bool,
    pub use_title: bool,
    pub include_user_message: bool,
    pub include_agent_preview: bool,
    pub preview_length: u64,
    /// Terminal states that trigger a notification, e.g. ["completed"].
    pub terminal_states: Vec<String>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            use_title: true,
            include_user_message: true,
            include_agent_preview: false,
            preview_length: 160,
            terminal_states: vec!["completed".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardSettings {
    pub show_latest_prompt: bool,
    pub show_agent_preview: bool,
    pub preview_length: u64,
    pub show_cwd: bool,
    pub show_runtime: bool,
    pub show_stats: bool,
    pub show_model: bool,
}

impl Default for DashboardSettings {
    fn default() -> Self {
        Self {
            show_latest_prompt: true,
            show_agent_preview: false,
            preview_length: 200,
            show_cwd: true,
            show_runtime: true,
            show_stats: true,
            show_model: true,
        }
    }
}

impl Settings {
    /// Sanitize inbound values: clamp lengths, drop invalid terminal states.
    pub fn sanitized(mut self) -> Self {
        self.notifications.preview_length = self.notifications.preview_length.clamp(0, MAX_PREVIEW_LENGTH);
        self.dashboard.preview_length = self.dashboard.preview_length.clamp(0, MAX_PREVIEW_LENGTH);
        self.notifications.terminal_states.retain(|s| {
            s == "completed" || s == "error" || s == "aborted" || s == "stopped"
        });
        if self.notifications.terminal_states.is_empty() {
            self.notifications.terminal_states = vec!["completed".to_string()];
        }
        self.version = SETTINGS_VERSION;
        self
    }

    pub fn load(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("settings parse error ({e}); using defaults");
                    let bad = path.with_extension("json.bad");
                    let _ = fs::rename(path, &bad);
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_spec_conformant() {
        let s = Settings::default();
        assert!(s.notifications.enabled);
        assert!(s.notifications.use_title);
        assert!(s.notifications.include_user_message);
        assert!(!s.notifications.include_agent_preview);
        assert_eq!(s.notifications.preview_length, 160);
        assert_eq!(s.notifications.terminal_states, vec!["completed"]);
    }

    #[test]
    fn sanitize_clamps_and_filters() {
        let mut s = Settings::default();
        s.notifications.preview_length = 99999;
        s.dashboard.preview_length = 99999;
        s.notifications.terminal_states = vec!["completed".into(), "bogus".into()];
        let s = s.sanitized();
        assert_eq!(s.notifications.preview_length, MAX_PREVIEW_LENGTH as u64);
        assert_eq!(s.dashboard.preview_length, MAX_PREVIEW_LENGTH as u64);
        assert_eq!(s.notifications.terminal_states, vec!["completed"]);
    }

    #[test]
    fn roundtrip_atomic_save_load() {
        let dir = std::env::temp_dir().join(format!("omp-ntfy-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let mut s = Settings::default();
        s.notifications.include_agent_preview = true;
        s.save(&path).unwrap();
        let loaded = Settings::load(&path);
        assert!(loaded.notifications.include_agent_preview);
        assert_eq!(loaded.notifications.preview_length, 160);
        std::fs::remove_dir_all(&dir).ok();
    }
}