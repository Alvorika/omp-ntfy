use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::push::{self, PushResult};
use crate::store::{now_unix, StoredEvent, Store, Subscription};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub store: Arc<Mutex<Store>>,
    pub http: reqwest::Client,
}

pub fn build(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/events", post(handle_events))
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
#[allow(dead_code)] // contract fields from the extension; validated but not yet rendered
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
    if body.len() > 16 * 1024 {
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

    let subscribers = {
        let mut g = st.store.lock().await;
        g.push_event(stored.clone());
        if let Err(e) = g.save() {
            tracing::warn!("store save failed: {e}");
        }
        g.data.subscriptions.len()
    };

    // Never let push delivery block the OMP extension's short HTTP timeout.
    tokio::spawn(fanout(st, stored));

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "accepted": true, "subscribers": subscribers })),
    )
        .into_response()
}

async fn fanout(st: AppState, event: StoredEvent) {
    let subs = { st.store.lock().await.data.subscriptions.clone() };
    if subs.is_empty() {
        tracing::info!("event accepted, no subscribers");
        return;
    }
    let msg = serde_json::to_vec(&notification_for(&event))
        .unwrap_or_else(|_| br#"{"title":"OMP","body":"Task completed"}"#.to_vec());

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

fn notification_for(ev: &StoredEvent) -> serde_json::Value {
    let name = ev
        .session_name
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| ev.cwd.as_ref().map(|c| basename(c)))
        .unwrap_or_else(|| "session".to_string());
    let (title, body) = if ev.event == "test" {
        ("OMP".to_string(), "Test notification".to_string())
    } else {
        let body = match ev.duration_min {
            Some(d) if d > 0 => format!("Task completed · {d}m"),
            _ => "Task completed".to_string(),
        };
        (format!("OMP · {name}"), body)
    };
    serde_json::json!({ "title": title, "body": body, "tag": "omp-ntfy", "url": "/", "ts": ev.ts })
}

fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
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
    if body.len() > 16 * 1024 {
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