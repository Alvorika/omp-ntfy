use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::config::{Config, Paths};
use crate::omp_sessions::{self, PersistentSession};
use crate::push::{self, PushResult};
use crate::settings::Settings;
use crate::stats::StatsSource;
use crate::store::{now_unix, RuntimeRecord, SessionRecord, StoredEvent, Store, Subscription};

const MAX_RUNTIME_BODY: usize = 16 * 1024;
const MAX_TITLE: usize = 128;
const MAX_CWD: usize = 512;
const MAX_MODEL: usize = 128;
const MAX_PROMPT: usize = 500;
const MAX_PREVIEW: usize = 1000;
const MAX_DETAIL: usize = 120;
const SCAN_TTL: Duration = Duration::from_secs(30);

/// Detect the OMP agent directory: PI_CODING_AGENT_DIR override, else ~/.omp/agent.
pub fn detect_agent_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PI_CODING_AGENT_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".omp/agent"))
        .unwrap_or_else(|_| PathBuf::from(".omp/agent"))
}

pub struct SessionScanner {
    agent_dir: PathBuf,
    omp_home: PathBuf,
    last_scan: Option<Instant>,
    cache: Vec<PersistentSession>,
    runtime_clients: Vec<(u32, String)>,
    active_files: Vec<PathBuf>,
}

impl SessionScanner {
    pub fn new() -> Self {
        let agent_dir = detect_agent_dir();
        let omp_home = agent_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| agent_dir.clone());
        Self {
            agent_dir,
            omp_home,
            last_scan: None,
            cache: Vec::new(),
            runtime_clients: Vec::new(),
            active_files: Vec::new(),
        }
    }

    pub fn refresh(&mut self) {
        if self.last_scan.is_some_and(|t| t.elapsed() < SCAN_TTL) {
            return;
        }
        self.cache = omp_sessions::scan_sessions(&self.agent_dir);
        self.runtime_clients = omp_sessions::running_clients(&self.omp_home)
            .into_iter()
            .filter(|(pid, _)| omp_sessions::is_pid_alive(*pid))
            .collect();
        self.active_files = omp_sessions::active_session_files(&self.omp_home);
        self.last_scan = Some(Instant::now());
        tracing::debug!(
            "scanned {} sessions, {} live clients",
            self.cache.len(),
            self.runtime_clients.len()
        );
    }

    pub fn sessions(&mut self) -> Vec<PersistentSession> {
        self.refresh();
        self.cache.clone()
    }

    pub fn running(&mut self) -> Vec<(u32, String)> {
        self.refresh();
        self.runtime_clients.clone()
    }

    pub fn active_terminal_count(&mut self) -> usize {
        self.refresh();
        self.active_files.len()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<Mutex<Store>>,
    pub settings: Arc<Mutex<Settings>>,
    pub stats: Arc<Mutex<StatsSource>>,
    pub paths: Arc<Paths>,
    pub scanner: Arc<Mutex<SessionScanner>>,
    pub http: reqwest::Client,
}

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/events", post(handle_events))
        .route("/api/runtime", post(handle_runtime))
        .route("/api/sessions", get(handle_sessions))
        .route("/api/stats", get(handle_stats))
        .route("/api/settings", get(handle_settings_get).put(handle_settings_put))
        .route("/api/omp-binding", get(handle_binding))
        .route("/api/subscribe", post(handle_subscribe).delete(handle_unsubscribe))
        .route("/api/status", get(handle_status))
        .route("/api/vapid-public", get(handle_vapid))
        .route("/", get(|| async { static_file("/") }))
        .fallback(|uri: Uri| async move { static_file(uri.path()) })
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

// ------------------------------------------------------------------ /api/events

#[derive(Deserialize)]
#[allow(dead_code)] // contract fields from the extension; validated but not all rendered
struct EventPayload {
    event: String,
    session_id: Option<String>,
    session_name: Option<String>,
    cwd: Option<String>,
    hostname: Option<String>,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    duration_min: Option<u64>,
    turn_id: Option<u64>,
    outcome: Option<String>,
}

async fn handle_events(State(st): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !authorized(&headers, &st.cfg.token) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if body.len() > MAX_RUNTIME_BODY {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }
    let payload: EventPayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
    };
    let event = payload.event.trim();
    if event.is_empty() || event.len() > 32 || !event.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return error(StatusCode::BAD_REQUEST, "invalid event");
    }
    let bounded = |s: Option<String>, n: usize| s.filter(|v| v.len() <= n);
    let stored = StoredEvent {
        event: event.to_string(),
        ts: now_unix(),
        session_name: bounded(payload.session_name, 128),
        hostname: bounded(payload.hostname, 256),
        cwd: bounded(payload.cwd, 512),
        duration_min: payload.duration_min.filter(|d| *d <= 100_000),
    };

    let session_id = payload.session_id.filter(|s| !s.is_empty());

    let (subscribers, notification) = {
        let mut g = st.store.lock().await;
        let sess = session_id.as_deref().and_then(|id| g.get_session(id).cloned());
        g.push_event(stored.clone());
        if let Err(e) = g.save() {
            tracing::warn!("store save failed: {e}");
        }
        let settings = st.settings.lock().await.clone();
        let n = g.data.subscriptions.len();
        (n, build_notification(&settings, &stored, sess.as_ref()))
    };

    if let Some(msg) = notification {
        tokio::spawn(fanout(st, stored, msg));
    } else {
        tracing::info!("event accepted; notification suppressed by settings");
    }

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true, "subscribers": subscribers })),
    )
        .into_response()
}

/// Assemble the notification JSON, honoring settings. None = suppressed.
fn build_notification(
    settings: &Settings,
    ev: &StoredEvent,
    session: Option<&SessionRecord>,
) -> Option<Vec<u8>> {
    let n = &settings.notifications;
    if !n.enabled {
        return None;
    }
    // terminal-state gating: events carry no explicit outcome in the store;
    // "session_stop" maps to the "completed" terminal state.
    let event_state = if ev.event == "session_stop" { "completed" } else { ev.event.as_str() };
    if ev.event != "test" && !n.terminal_states.iter().any(|s| s == event_state) {
        return None;
    }

    let (title, body) = if ev.event == "test" {
        ("OMP".to_string(), "Test notification".to_string())
    } else {
        let title = if n.use_title {
            session
                .and_then(|s| s.title.clone())
                .filter(|t| !t.is_empty())
                .map(|t| format!("OMP · {t}"))
                .unwrap_or_else(|| {
                    let name = ev
                        .session_name
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| ev.cwd.as_ref().map(|c| basename(c)))
                        .unwrap_or_else(|| "OMP".to_string());
                    if name == "OMP" {
                        name
                    } else {
                        format!("OMP · {name}")
                    }
                })
        } else {
            "OMP".to_string()
        };
        let len = n.preview_length as usize;
        let prompt = n
            .include_user_message
            .then(|| session.and_then(|s| s.latest_user_prompt.clone()))
            .flatten()
            .filter(|p| !p.is_empty())
            .map(|p| clip(&p, len));
        let preview = n
            .include_agent_preview
            .then(|| session.and_then(|s| s.latest_assistant_preview.clone()))
            .flatten()
            .filter(|p| !p.is_empty())
            .map(|p| clip(&p, len));
        let body = match (preview, prompt) {
            (Some(p), Some(q)) => format!("{p} — {q}"),
            (Some(p), None) => p,
            (None, Some(q)) => q,
            (None, None) => match ev.duration_min {
                Some(d) if d > 0 => format!("Task completed · {d}m"),
                _ => "Task completed".to_string(),
            },
        };
        (title, body)
    };

    Some(
        serde_json::json!({ "title": title, "body": body, "tag": "omp-ntfy", "url": "/", "ts": ev.ts })
            .to_string()
            .into_bytes(),
    )
}

async fn fanout(st: AppState, _event: StoredEvent, msg: Vec<u8>) {
    let subs = { st.store.lock().await.data.subscriptions.clone() };
    if subs.is_empty() {
        tracing::info!("event accepted, no subscribers");
        return;
    }
    let mut sent = 0usize;
    let mut gone = 0usize;
    for sub in &subs {
        match push::send_push(&st.http, &st.cfg, sub, &msg).await {
            Ok(PushResult::Sent) => {
                sent += 1;
        let mut g = st.store.lock().await;
                if let Some(s) = g.data.subscriptions.iter_mut().find(|s| s.endpoint == sub.endpoint) {
                    s.last_pushed_at = Some(now_unix());
                    s.fail_count = 0;
                }
                let _ = g.save();
            }
            Ok(PushResult::Gone) => {
                gone += 1;
                tracing::warn!("subscription gone, pruning: {}", sub.endpoint);
        let mut g = st.store.lock().await;
                g.data.subscriptions.retain(|s| s.endpoint != sub.endpoint);
                let _ = g.save();
            }
            Err(e) => {
                tracing::warn!("push failed for {}: {e}", sub.endpoint);
        let mut g = st.store.lock().await;
                if let Some(s) = g.data.subscriptions.iter_mut().find(|s| s.endpoint == sub.endpoint) {
                    s.fail_count += 1;
                }
                let _ = g.save();
            }
        }
    }
    tracing::info!(sent, gone, total = subs.len(), "push fanout done");
}

fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
}

// ---------------------------------------------------------------- /api/runtime

#[derive(Deserialize)]
struct RuntimeUpdate {
    session_id: String,
    title: Option<String>,
    cwd: Option<String>,
    hostname: Option<String>,
    model: Option<String>,
    latest_user_prompt: Option<String>,
    latest_assistant_preview: Option<String>,
    activity: Option<String>,
    activity_detail: Option<String>,
    state: Option<String>,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    ts: Option<u64>,
}

async fn handle_runtime(State(st): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !authorized(&headers, &st.cfg.token) {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if body.len() > MAX_RUNTIME_BODY {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }
    let u: RuntimeUpdate = match serde_json::from_str(&body) {
        Ok(u) => u,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
    };
    let session_id = clip(&u.session_id, MAX_TITLE);
    if session_id.is_empty() {
        return error(StatusCode::BAD_REQUEST, "session_id required");
    }
    let ts = u.ts.unwrap_or_else(now_unix);
    let state = match u.state.as_deref() {
        Some("completed") | Some("error") => u.state.unwrap(),
        _ => "active".to_string(),
    };

        let mut g = st.store.lock().await;
    let mut rec = match g.get_session(&session_id).cloned() {
        Some(existing) => existing,
        None => SessionRecord::new(session_id.clone()),
    };
    if let Some(t) = u.title.filter(|t| !t.is_empty()) {
        rec.title = Some(clip(&t, MAX_TITLE));
    }
    if let Some(c) = u.cwd.filter(|c| !c.is_empty()) {
        rec.cwd = Some(clip(&c, MAX_CWD));
    }
    if let Some(h) = u.hostname.filter(|h| !h.is_empty()) {
        rec.hostname = Some(clip(&h, 256));
    }
    if let Some(m) = u.model.filter(|m| !m.is_empty()) {
        rec.model = Some(clip(&m, MAX_MODEL));
    }
    if let Some(p) = u.latest_user_prompt.filter(|p| !p.is_empty()) {
        rec.latest_user_prompt = Some(clip(&p, MAX_PROMPT));
    }
    if let Some(p) = u.latest_assistant_preview.filter(|p| !p.is_empty()) {
        rec.latest_assistant_preview = Some(clip(&p, MAX_PREVIEW));
    }
    if state != "active" {
        rec.state = state.clone();
        if u.finished_at.is_some() {
            rec.ended_at = u.finished_at;
        } else if rec.ended_at.is_none() {
            rec.ended_at = Some(ts);
        }
    }
    if u.started_at.is_some() {
        rec.started_at = u.started_at;
    }
    rec.last_seen_at = ts;

    if state == "active" {
        // Keep the runtime entry alive only while the session is active;
        // terminal updates remove it so stale live state never lingers.
        rec.state = "active".to_string();
        g.runtime.insert(
            session_id.clone(),
            RuntimeRecord {
                session_id: session_id.clone(),
                activity: u.activity.filter(|a| !a.is_empty()),
                activity_detail: u.activity_detail.map(|d| clip(&d, MAX_DETAIL)),
                last_seen_at: ts,
            },
        );
    } else {
        rec.state = state;
        g.runtime.remove(&session_id);
    }
    g.upsert_session(rec);
    if let Err(e) = g.save() {
        tracing::warn!("store save failed: {e}");
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true })),
    )
        .into_response()
}

// --------------------------------------------------------------- /api/sessions

async fn handle_sessions(State(st): State<AppState>) -> Response {
    let (scanned, running, active_terms) = {
        let mut sc = st.scanner.lock().await;
        (sc.sessions(), sc.running(), sc.active_terminal_count())
    };
        let mut g = st.store.lock().await;
    // Fold disk truth into the store (without disturbing terminal states).
    let now = now_unix();
    for p in &scanned {
        let rec = SessionRecord {
            session_id: p.session_id.clone(),
            title: p.title.clone().filter(|t| !t.is_empty()),
            cwd: p.cwd.clone(),
            hostname: None,
            model: None,
            latest_user_prompt: p.latest_user_prompt.clone().filter(|t| !t.is_empty()),
            latest_assistant_preview: p.latest_assistant_preview.clone().filter(|t| !t.is_empty()),
            state: "active".to_string(), // never overwrite terminal state
            started_at: p.created_at,
            ended_at: None,
            last_seen_at: p.updated_at.max(now),
        };
        g.upsert_session(rec);
    }
    if let Err(e) = g.save() {
        tracing::warn!("store save failed: {e}");
    }
    // Prune runtime entries that vanished (finalized or host restarted) and
    // stale live records (> 1h without an update).
    let live_ids: std::collections::HashSet<String> =
        g.data.sessions.iter().map(|s| s.session_id.clone()).collect();
    g.runtime.retain(|id, r| {
        live_ids.contains(id) && (now - r.last_seen_at) < 3600
    });
    let mut sessions: Vec<_> = g.data.sessions.iter().cloned().collect();
    let runtime: Vec<_> = g.runtime.values().cloned().collect();
    drop(g);
    sessions.sort_by(|a, b| b.last_seen_at.cmp(&a.last_seen_at));
    let server_time = now;
    (
        Json(serde_json::json!({
            "sessions": sessions,
            "runtime": runtime,
            "server_time": server_time,
            "running_processes": running.iter().map(|(pid, dir)| serde_json::json!({ "pid": pid, "project_dir": dir })).collect::<Vec<_>>(),
            "active_terminals": active_terms,
        })),
    )
        .into_response()
}

// ------------------------------------------------------------------ /api/stats

async fn handle_stats(State(st): State<AppState>) -> Response {
    let summary = st.stats.lock().await.summary();
    match summary {
        Some(s) => (
            Json(serde_json::json!({
                "available": true,
                "requests": s.requests,
                "tokens_in": s.tokens_in,
                "tokens_out": s.tokens_out,
                "cache_read": s.cache_read,
                "cost_usd": s.cost_usd,
                "subagent_requests": s.subagent_requests,
                "last_record_ts": s.last_record_ts,
            })),
        )
            .into_response(),
        None => (Json(serde_json::json!({ "available": false }))).into_response(),
    }
}

// ---------------------------------------------------------------- /api/settings

async fn handle_settings_get(State(st): State<AppState>) -> Response {
    let s = st.settings.lock().await.clone();
    (Json(s)).into_response()
}

async fn handle_settings_put(State(st): State<AppState>, body: String) -> Response {
    if body.len() > MAX_RUNTIME_BODY {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }
    let incoming: Settings = match serde_json::from_str(&body) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid settings: {e}")),
    };
    let s = incoming.sanitized();
    if let Err(e) = s.save(&st.paths.settings_file()) {
        tracing::warn!("settings save failed: {e}");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "persist failed");
    }
    let mut g = st.settings.lock().await;
    *g = s.clone();
    (Json(serde_json::json!({ "ok": true, "settings": s }))).into_response()
}

// ------------------------------------------------------------- /api/omp-binding

async fn handle_binding(State(st): State<AppState>) -> Response {
    let agent_dir = detect_agent_dir();
    let omp_home = agent_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| agent_dir.clone());

    let ext = agent_dir.join("extensions/omp-ntfy.ts");
    let extension_installed = ext.exists();
    let extension_version = if extension_installed {
        let text = std::fs::read_to_string(&ext).unwrap_or_default();
        text.lines()
            .find(|l| l.contains("OMP_NTFY_EXT_VERSION"))
            .and_then(|l| l.split('"').nth(1))
            .map(|v| v.to_string())
    } else {
        None
    };

    let client_json = std::fs::read_to_string(st.paths.client_file())
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let client_url_ok = client_json
        .as_ref()
        .and_then(|v| v.get("url").and_then(|u| u.as_str()))
        .is_some_and(|u| !u.is_empty());
    let client_token_ok = client_json
        .as_ref()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()))
        .is_some_and(|t| !t.is_empty());

    (
        Json(serde_json::json!({
            "home": omp_home.display().to_string(),
            "agent_dir": agent_dir.display().to_string(),
            "extension_installed": extension_installed,
            "extension_version": extension_version,
            "client_url_ok": client_url_ok,
            "client_token_ok": client_token_ok,
            "server_version": env!("CARGO_PKG_VERSION"),
        })),
    )
        .into_response()
}

// -------------------------------------------------------------- /api/subscribe

#[derive(Deserialize)]
struct KeysPayload {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
struct SubscribePayload {
    endpoint: String,
    keys: KeysPayload,
    device_name: Option<String>,
}

async fn handle_subscribe(State(st): State<AppState>, body: String) -> Response {
    if body.len() > MAX_RUNTIME_BODY {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }
    let p: SubscribePayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
    };
    let endpoint = p.endpoint.trim().to_string();
    if !valid_endpoint(&endpoint) {
        return error(StatusCode::BAD_REQUEST, "endpoint must be https:// (http:// loopback allowed)");
    }
    let p256dh_ok = matches!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&p.keys.p256dh),
        Ok(b) if b.len() == 65 && b[0] == 0x04
    );
    if !p256dh_ok {
        return error(StatusCode::BAD_REQUEST, "keys.p256dh must be a 65-byte uncompressed P-256 point");
    }
    let auth_ok = matches!(
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&p.keys.auth),
        Ok(b) if b.len() == 16
    );
    if !auth_ok {
        return error(StatusCode::BAD_REQUEST, "keys.auth must be 16 bytes");
    }
    let device_name = p
        .device_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 40);

    let sub = Subscription {
        endpoint: endpoint.clone(),
        p256dh: p.keys.p256dh,
        auth: p.keys.auth,
        device_name,
        created_at: now_unix(),
        last_pushed_at: None,
        fail_count: 0,
    };
        let mut g = st.store.lock().await;
    if let Some(existing) = g.data.subscriptions.iter_mut().find(|s| s.endpoint == endpoint) {
        *existing = sub;
    } else {
        g.data.subscriptions.push(sub);
    }
    if let Err(e) = g.save() {
        tracing::warn!("store save failed: {e}");
    }
    let n = g.data.subscriptions.len();
    (
        StatusCode::CREATED,
        Json(serde_json::json!({ "ok": true, "subscribers": n })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct UnsubscribePayload {
    endpoint: String,
}

async fn handle_unsubscribe(State(st): State<AppState>, body: String) -> Response {
    let p: UnsubscribePayload = match serde_json::from_str(&body) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, &format!("invalid json: {e}")),
    };
        let mut g = st.store.lock().await;
    let before = g.data.subscriptions.len();
    g.data.subscriptions.retain(|s| s.endpoint != p.endpoint);
    let removed = before != g.data.subscriptions.len();
    if removed {
        let _ = g.save();
    }
    (Json(serde_json::json!({ "ok": true, "removed": removed }))).into_response()
}

// --------------------------------------------------------------------- status

async fn handle_status(State(st): State<AppState>) -> Response {
    let g = st.store.lock().await;
    (
        Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "subscribers": g.data.subscriptions.len(),
            "sessions": g.data.sessions.len(),
            "last_event": g.data.events.last(),
        })),
    )
        .into_response()
}

async fn handle_vapid(State(st): State<AppState>) -> Response {
    match st.cfg.vapid_public_key() {
        Ok(key) => (
            Json(serde_json::json!({ "key": key, "subject": st.cfg.vapid_subject })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("cannot derive VAPID public key: {e}");
            error(StatusCode::INTERNAL_SERVER_ERROR, "vapid key unavailable")
        }
    }
}

// --------------------------------------------------------------------- static

fn static_file(path: &str) -> Response {
    let (body, ct, nocache) = match path {
        "/" | "/index.html" => (include_bytes!("../web/index.html").as_slice(), "text/html; charset=utf-8", false),
        "/app.js" => (include_bytes!("../web/app.js").as_slice(), "application/javascript; charset=utf-8", false),
        "/sw.js" => (include_bytes!("../web/sw.js").as_slice(), "application/javascript; charset=utf-8", true),
        "/manifest.webmanifest" => (include_bytes!("../web/manifest.webmanifest").as_slice(), "application/manifest+json", true),
        "/style.css" => (include_bytes!("../web/style.css").as_slice(), "text/css; charset=utf-8", false),
        "/icon-192.png" => (include_bytes!("../web/icon-192.png").as_slice(), "image/png", false),
        "/icon-512.png" => (include_bytes!("../web/icon-512.png").as_slice(), "image/png", false),
        "/apple-touch-icon.png" => (include_bytes!("../web/apple-touch-icon.png").as_slice(), "image/png", false),
        _ => return error(StatusCode::NOT_FOUND, "not found"),
    };
    let mut resp = ([(header::CONTENT_TYPE, ct)], body).into_response();
    if nocache {
        resp.headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    resp
}

// --------------------------------------------------------------------- helpers

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(v) = headers.get(header::AUTHORIZATION) else { return false };
    let Ok(v) = v.to_str() else { return false };
    let mut parts = v.splitn(2, ' ');
    if !parts.next().unwrap_or("").eq_ignore_ascii_case("bearer") {
        return false;
    }
    let cred = parts.next().unwrap_or("");
    constant_time_eq(cred.as_bytes(), token.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn valid_endpoint(e: &str) -> bool {
    let url = match reqwest::Url::parse(e) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match url.scheme() {
        "https" => true,
        "http" => matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1" | "[::1]")),
        _ => false,
    }
}

fn clip(s: &str, n: usize) -> String {
    if s.len() > n {
        s.chars().take(n).collect()
    } else {
        s.to_string()
    }
}

#[allow(dead_code)]
fn _unused_path(p: &Path) -> String {
    p.display().to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use crate::store::SessionRecord;

    fn session() -> SessionRecord {
        SessionRecord {
            session_id: "s1".into(),
            title: Some("deploy staging".into()),
            cwd: Some("/home/u/work".into()),
            hostname: None,
            model: Some("p/m".into()),
            latest_user_prompt: Some("deploy the staging environment".into()),
            latest_assistant_preview: Some("Deployed staging successfully.".into()),
            state: "completed".into(),
            started_at: Some(1000),
            ended_at: Some(2000),
            last_seen_at: 2000,
        }
    }

    fn ev() -> StoredEvent {
        StoredEvent {
            event: "session_stop".into(),
            ts: 2000,
            session_name: None,
            hostname: None,
            cwd: None,
            duration_min: Some(12),
        }
    }

    fn parse(b: &[u8]) -> serde_json::Value {
        serde_json::from_slice(b).unwrap()
    }

    #[test]
    fn defaults_use_title_and_prompt() {
        let s = Settings::default();
        let n = build_notification(&s, &ev(), Some(&session())).unwrap();
        let v = parse(&n);
        assert_eq!(v["title"], "OMP · deploy staging");
        assert_eq!(v["body"], "deploy the staging environment");
    }

    #[test]
    fn preview_disabled_by_default_absent_preview() {
        let mut s = Settings::default();
        s.notifications.include_agent_preview = true;
        let sess = session();
        let sess_no_preview = SessionRecord { latest_assistant_preview: None, ..sess };
        let n = build_notification(&s, &ev(), Some(&sess_no_preview)).unwrap();
        assert_eq!(parse(&n)["body"], "deploy the staging environment");
    }

    #[test]
    fn preview_when_enabled() {
        let mut s = Settings::default();
        s.notifications.include_agent_preview = true;
        let n = build_notification(&s, &ev(), Some(&session())).unwrap();
        assert_eq!(parse(&n)["body"], "Deployed staging successfully. — deploy the staging environment");
    }

    #[test]
    fn preview_length_clips() {
        let mut s = Settings::default();
        s.notifications.preview_length = 10;
        let n = build_notification(&s, &ev(), Some(&session())).unwrap();
        assert_eq!(parse(&n)["body"], "deploy the");
    }

    #[test]
    fn disabled_notifications_suppress() {
        let mut s = Settings::default();
        s.notifications.enabled = false;
        assert!(build_notification(&s, &ev(), Some(&session())).is_none());
    }

    #[test]
    fn test_event_unchanged() {
        let s = Settings::default();
        let t = StoredEvent { event: "test".into(), ..ev() };
        let n = build_notification(&s, &t, None).unwrap();
        let v = parse(&n);
        assert_eq!(v["title"], "OMP");
        assert_eq!(v["body"], "Test notification");
    }

    #[test]
    fn fallback_when_no_session() {
        let s = Settings::default();
        let n = build_notification(&s, &ev(), None).unwrap();
        let v = parse(&n);
        assert_eq!(v["title"], "OMP");
        assert_eq!(v["body"], "Task completed · 12m");
    }

    #[test]
    fn terminal_states_gate() {
        let mut s = Settings::default();
        s.notifications.terminal_states = vec!["error".into()];
        assert!(build_notification(&s, &ev(), Some(&session())).is_none());
    }
}
