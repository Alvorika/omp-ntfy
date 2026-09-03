mod config;
mod omp;
mod omp_sessions;
mod push;
mod service;
mod settings;
mod stats;
mod store;
mod server;
use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

use config::{Config, Paths};

#[derive(Parser)]
#[command(name = "omp-ntfy", version, about = "Web Push notifications when an Oh My Pi agent finishes")]
struct Cli {
/// Config directory (default: $XDG_CONFIG_HOME/omp-ntfy)
#[arg(long, global = true)]
config_dir: Option<PathBuf>,
/// Data directory (default: $XDG_STATE_HOME/omp-ntfy)
#[arg(long, global = true)]
data_dir: Option<PathBuf>,
#[command(subcommand)]
cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
/// Create config + VAPID keys (first run)
Init {
/// Bearer token to require (default: random)
#[arg(long)]
token: Option<String>,
/// VAPID subject, e.g. mailto:you@example.com
#[arg(long)]
subject: Option<String>,
/// Listen address (default 127.0.0.1:8090)
#[arg(long)]
bind: Option<String>,
/// Overwrite an existing config
#[arg(long)]
force: bool,
},
/// Run the notification server
Serve,
/// Show the bearer token
Token,
/// Rotate the bearer token
TokenRotate,
/// List subscribed devices
Subs,
/// Remove a subscribed device (index from `omp-ntfy subs`)
SubsRm { index: usize },
/// Send a test notification to all subscribed devices
PushTest {
/// Server base URL (default: derived from config bind)
#[arg(long)]
url: Option<String>,
},
/// Manage the systemd user service
Service {
#[command(subcommand)]
cmd: ServiceCmd,
},
/// Install/update/remove the OMP extension and client config
Omp {
#[command(subcommand)]
cmd: OmpCmd,
},
}

#[derive(Subcommand)]
enum ServiceCmd {
/// Write the unit, enable and start it
Install,
/// Stop, disable and remove the unit
Uninstall,
Start,
Stop,
Restart,
Status,
}

#[derive(Subcommand)]
enum OmpCmd {
/// Install the extension into ~/.omp/agent/extensions + write client config
Install {
/// Server base URL, e.g. http://host:8090
#[arg(long)]
url: Option<String>,
/// Bearer token
#[arg(long)]
token: Option<String>,
/// Install for an omp profile (~/.omp/profiles/<name>/agent)
#[arg(long)]
profile: Option<String>,
/// Override the agent directory (PI_CODING_AGENT_DIR)
#[arg(long)]
agent_dir: Option<PathBuf>,
},
/// Re-copy the extension after an omp-ntfy upgrade
Update {
#[arg(long)]
profile: Option<String>,
#[arg(long)]
agent_dir: Option<PathBuf>,
},
/// Show whether the extension is installed
Status {
#[arg(long)]
profile: Option<String>,
#[arg(long)]
agent_dir: Option<PathBuf>,
},
/// Remove the extension (client config is kept)
Uninstall {
#[arg(long)]
profile: Option<String>,
#[arg(long)]
agent_dir: Option<PathBuf>,
},
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
tracing_subscriber::fmt()
.with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
.init();

let cli = Cli::parse();
let paths = Paths::from_env(cli.config_dir.as_ref(), cli.data_dir.as_ref());

match cli.cmd {
Cmd::Init { token, subject, bind, force } => {
if paths.config_file().exists() && !force {
anyhow::bail!(
"{} already exists (use --force to overwrite)",
paths.config_file().display()
);
}
let cfg = Config::generate(token, subject, bind)?;
cfg.save(&paths.config_file())?;
println!("config  → {}", paths.config_file().display());
println!("data    → {}", paths.store_file().display());
println!("bind    → {}", cfg.bind);
println!("token   → {}", cfg.token);
println!("vapid public key → {}", cfg.vapid_public_key()?);
println!("subject → {}", cfg.vapid_subject);
println!("\nRun `omp-ntfy serve`, then install the PWA and run `omp-ntfy omp install --url http://<host>:<port> --token <token>` on each OMP machine.");
}
Cmd::Serve => {
            let cfg = Arc::new(Config::load(&paths.config_file())?);
            let store = Arc::new(Mutex::new(store::Store::load(paths.store_file())));
            let settings = Arc::new(Mutex::new(settings::Settings::load(&paths.settings_file())));
            let stats = Arc::new(Mutex::new(stats::StatsSource::new(
                std::env::var("HOME")
                    .map(|h| std::path::PathBuf::from(h).join(".omp/stats.db"))
                    .unwrap_or_else(|_| std::path::PathBuf::from("stats.db")),
            )));
            let scanner = Arc::new(Mutex::new(server::SessionScanner::new()));
            let paths = Arc::new(paths);
            let http = reqwest::Client::builder()
                .user_agent(format!("omp-ntfy/{}", env!("CARGO_PKG_VERSION")))
                .build()?;
            let app = server::build(server::AppState {
                cfg: cfg.clone(),
                store,
                settings,
                stats,
                paths,
                scanner,
                http,
            });
            let listener = tokio::net::TcpListener::bind(&cfg.bind).await
                .with_context(|| format!("cannot bind {}", cfg.bind))?;
            tracing::info!("omp-ntfy listening on http://{}", cfg.bind);
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await
                .context("server error")?;
            tracing::info!("server stopped");
        }
        Cmd::Token => {
let cfg = Config::load(&paths.config_file())?;
println!("{}", cfg.token);
}
Cmd::TokenRotate => {
let mut cfg = Config::load(&paths.config_file())?;
cfg.token = Config::generate(None, None, None)?.token;
cfg.save(&paths.config_file())?;
println!("new token: {}", cfg.token);
}
Cmd::Subs => {
let store = store::Store::load(paths.store_file());
if store.data.subscriptions.is_empty() {
println!("no subscriptions");
return Ok(());
}
for (i, s) in store.data.subscriptions.iter().enumerate() {
println!(
"[{i}] {} {:?} last={:?} fails={}",
s.endpoint,
s.device_name.as_deref().unwrap_or(""),
s.last_pushed_at,
s.fail_count
);
}
}
Cmd::SubsRm { index } => {
let mut store = store::Store::load(paths.store_file());
if index >= store.data.subscriptions.len() {
anyhow::bail!("index {index} out of range ({} subscriptions)", store.data.subscriptions.len());
}
let removed = store.data.subscriptions.remove(index);
store.save()?;
println!("removed: {}", removed.endpoint);
}
Cmd::PushTest { url } => {
let cfg = Config::load(&paths.config_file())?;
let base = url.unwrap_or_else(|| {
let host = cfg.bind.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1");
format!("http://{host}:{}", cfg.bind.rsplit_once(':').map(|(_, p)| p).unwrap_or("8090"))
});
let client = reqwest::Client::new();
let resp = client
.post(format!("{}/api/events", base.trim_end_matches('/')))
.bearer_auth(&cfg.token)
.header("content-type", "application/json")
.body(serde_json::to_vec(&serde_json::json!({ "event": "test", "finished_at": store::now_unix() }))?)
.timeout(std::time::Duration::from_secs(5))
.send()
.await
.context("request failed (is the server running?)")?;
println!("{} {}", resp.status(), resp.text().await.unwrap_or_default());
}
Cmd::Service { cmd } => {
let bin = std::env::current_exe().context("cannot resolve own binary path")?;
match cmd {
ServiceCmd::Install => service::install(&bin, &paths.config_dir, &paths.data_dir)?,
ServiceCmd::Uninstall => service::uninstall()?,
ServiceCmd::Start => passthrough(&["start", service::UNIT_NAME])?,
ServiceCmd::Stop => passthrough(&["stop", service::UNIT_NAME])?,
ServiceCmd::Restart => passthrough(&["restart", service::UNIT_NAME])?,
ServiceCmd::Status => passthrough(&["status", service::UNIT_NAME])?,
}
}
Cmd::Omp { cmd } => match cmd {
OmpCmd::Install { url, token, profile, agent_dir } => {
omp::install(&paths, url, token, profile, agent_dir)?
}
OmpCmd::Update { profile, agent_dir } => omp::update(profile, agent_dir)?,
OmpCmd::Status { profile, agent_dir } => omp::status(&paths, profile, agent_dir)?,
OmpCmd::Uninstall { profile, agent_dir } => omp::uninstall(profile, agent_dir)?,
},
}
Ok(())
}

fn passthrough(args: &[&str]) -> anyhow::Result<()> {
let st = service::systemctl(args)?;
std::process::exit(st.code().unwrap_or(1));
}

async fn shutdown_signal() {
let ctrl_c = async {
tokio::signal::ctrl_c().await.expect("ctrl_c handler");
};
#[cfg(unix)]
let term = async {
tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
.expect("SIGTERM handler")
.recv()
.await;
};
#[cfg(not(unix))]
let term = std::future::pending::<()>();
tokio::select! {
_ = ctrl_c => {},
_ = term => {},
}
tracing::info!("shutdown signal received");
}
