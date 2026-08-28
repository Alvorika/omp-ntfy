use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Paths;

const EXT_NAME: &str = "omp-ntfy.ts";
const EXT_VERSION: &str = "0.1.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    pub url: String,
    pub token: Option<String>,
}

fn agent_dir(profile: Option<&str>, agent_dir_flag: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(d) = agent_dir_flag {
        return Ok(d.to_path_buf());
    }
    if let Ok(d) = std::env::var("PI_CODING_AGENT_DIR") {
        if !d.is_empty() {
            return Ok(PathBuf::from(d));
        }
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(match profile {
        Some(p) => PathBuf::from(home).join(".omp/profiles").join(p).join("agent"),
        None => PathBuf::from(home).join(".omp/agent"),
    })
}

fn ext_path(agent: &Path) -> PathBuf {
    agent.join("extensions").join(EXT_NAME)
}

fn load_client(paths: &Paths) -> ClientConfig {
    fs::read_to_string(paths.client_file())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(ClientConfig { url: String::new(), token: None })
}

fn write_client(paths: &Paths, cfg: &ClientConfig) -> anyhow::Result<()> {
    fs::create_dir_all(&paths.config_dir)?;
    let tmp = paths.client_file().with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    fs::rename(&tmp, paths.client_file())?;
    Ok(())
}

pub fn install(
    paths: &Paths,
    url: Option<String>,
    token: Option<String>,
    profile: Option<String>,
    agent_dir_flag: Option<PathBuf>,
) -> anyhow::Result<()> {
    let mut cfg = load_client(paths);
    if let Some(u) = url {
        cfg.url = u;
    }
    if let Some(t) = token {
        cfg.token = Some(t);
    }
    if cfg.url.is_empty() {
        if let Ok(u) = std::env::var("OMP_NTFY_URL") {
            if !u.is_empty() {
                cfg.url = u;
            }
        }
    }
    if cfg.token.as_deref().unwrap_or("").is_empty() {
        if let Ok(t) = std::env::var("OMP_NTFY_TOKEN") {
            if !t.is_empty() {
                cfg.token = Some(t);
            }
        }
    }
    anyhow::ensure!(!cfg.url.is_empty(), "no server URL: pass --url, set OMP_NTFY_URL, or pre-fill {}", paths.client_file().display());
    anyhow::ensure!(!cfg.token.as_deref().unwrap_or("").is_empty(), "no bearer token: pass --token or set OMP_NTFY_TOKEN");
    write_client(paths, &cfg)?;

    let agent = agent_dir(profile.as_deref(), agent_dir_flag.as_deref())?;
    let dst = ext_path(&agent);
    fs::create_dir_all(dst.parent().unwrap())?;
    fs::write(&dst, include_str!("../ext/omp-ntfy.ts"))?;

    println!("extension  → {}", dst.display());
    println!("client cfg → {}", paths.client_file().display());
    println!("server     → {}", cfg.url);
    println!("installed (auto-discovered from {}). Verify with: omp-ntfy omp status", dst.parent().unwrap().display());
    Ok(())
}

pub fn update(profile: Option<String>, agent_dir_flag: Option<PathBuf>) -> anyhow::Result<()> {
    let agent = agent_dir(profile.as_deref(), agent_dir_flag.as_deref())?;
    let dst = ext_path(&agent);
    anyhow::ensure!(dst.exists(), "extension not installed at {}", dst.display());
    fs::write(&dst, include_str!("../ext/omp-ntfy.ts"))?;
    println!("updated {} to v{EXT_VERSION}", dst.display());
    Ok(())
}

pub fn status(paths: &Paths, profile: Option<String>, agent_dir_flag: Option<PathBuf>) -> anyhow::Result<()> {
    let agent = agent_dir(profile.as_deref(), agent_dir_flag.as_deref())?;
    let dst = ext_path(&agent);
    let installed = dst.exists();
    println!("agent dir   → {}", agent.display());
    println!("extension   → {} [{}]", dst.display(), if installed { "installed" } else { "not installed" });
    if installed {
        let text = fs::read_to_string(&dst).unwrap_or_default();
        println!("version     → {}", first_version(&text));
    }
    let cfg = load_client(paths);
    println!("client cfg  → {} [{}]", paths.client_file().display(), if cfg.url.is_empty() { "missing" } else { "ok" });
    if !cfg.url.is_empty() {
        println!("server url  → {}", cfg.url);
        println!("token       → {}", if cfg.token.as_deref().unwrap_or("").is_empty() { "missing" } else { "set" });
    }
    Ok(())
}

pub fn uninstall(profile: Option<String>, agent_dir_flag: Option<PathBuf>) -> anyhow::Result<()> {
    let agent = agent_dir(profile.as_deref(), agent_dir_flag.as_deref())?;
    let dst = ext_path(&agent);
    if dst.exists() {
        fs::remove_file(&dst)?;
        println!("removed {}", dst.display());
    }
    Ok(())
}

fn first_version(text: &str) -> &str {
    text.lines()
        .find(|l| l.contains("OMP_NTFY_EXT_VERSION"))
        .and_then(|l| l.split('"').nth(1))
        .unwrap_or("unknown")
}