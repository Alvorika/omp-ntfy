//! Read-only scanner for OMP persistent session transcripts.
//!
//! Transcripts live under `<agent_dir>/sessions/<cwd>/<ts>_<id>.jsonl` as one
//! JSON object per line. This module is safe to run while a live `omp`
//! process appends to the same files: every line is decoded independently,
//! so a half-written trailing line is simply skipped.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tracing::warn;

const MAX_SESSIONS: usize = 200;
const MAX_USER_PROMPT_CHARS: usize = 500;
const MAX_ASSISTANT_PREVIEW_CHARS: usize = 1000;

/// A snapshot of one persistent agent session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistentSession {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Epoch seconds when the session was created.
    pub created_at: Option<u64>,
    /// Epoch seconds of the newest record in the transcript (fallback: file mtime).
    pub updated_at: u64,
    pub message_count: u64,
    /// Latest user message text, stripped of leading `<...>` tag lines, max 500 chars.
    pub latest_user_prompt: Option<String>,
    /// First prose line of the latest assistant text, max 1000 chars.
    pub latest_assistant_preview: Option<String>,
}

/// Scan `<agent_dir>/sessions/**/*.jsonl` (session files directly in
/// `sessions/` plus one level of subdirectories) and return the sessions
/// newest-first. Never fails hard: unreadable files are logged and skipped.
pub fn scan_sessions(agent_dir: &Path) -> Vec<PersistentSession> {
    let sessions_dir = agent_dir.join("sessions");
    let mut result: Vec<PersistentSession> = Vec::new();

    let Ok(entries) = fs::read_dir(&sessions_dir) else {
        warn!("sessions dir unreadable: {}", sessions_dir.display());
        return result;
    };

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(inner) = fs::read_dir(&path) {
                for sub in inner.flatten() {
                    let p = sub.path();
                    if p.is_file() && matches_session_filename(&p) {
                        files.push(p);
                    }
                }
            }
        } else if path.is_file() && matches_session_filename(&path) {
            files.push(path);
        }
    }

    for file in files {
        match parse_session_file(&file) {
            Some(session) => result.push(session),
            None => warn!("skipping unreadable session file: {}", file.display()),
        }
    }

    result.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    result.truncate(MAX_SESSIONS);
    result
}

/// Parse one transcript file into a session snapshot. Returns `None` when the
/// file cannot be read or has no usable session record.
fn parse_session_file(path: &Path) -> Option<PersistentSession> {
    let file_name = path.file_name()?.to_str()?;
    let basename = file_name.strip_suffix(".jsonl")?;

    let raw = fs::read_to_string(path).ok()?;
    let mut session_id: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;
    let mut created_at: Option<u64> = None;
    let mut updated_at: u64 = 0;
    let mut message_count: u64 = 0;
    let mut latest_user_prompt: Option<String> = None;
    let mut latest_assistant: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A live append may leave a partial final line; skip it silently.
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let Some(record_type) = record.get("type").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let timestamp = record.get("timestamp").and_then(parse_timestamp);
        if let Some(ts) = timestamp {
            updated_at = updated_at.max(ts);
        }

        match record_type {
            "title" | "title_change" => {
                if let Some(t) = record.get("title").and_then(serde_json::Value::as_str) {
                    title = Some(t.to_string());
                }
            }
            "session" => {
                if let Some(id) = record.get("id").and_then(serde_json::Value::as_str) {
                    session_id = Some(id.to_string());
                }
                if cwd.is_none() {
                    if let Some(c) = record.get("cwd").and_then(serde_json::Value::as_str) {
                        cwd = Some(c.to_string());
                    }
                }
                if created_at.is_none() {
                    created_at = timestamp;
                }
                if title.is_none() {
                    if let Some(t) = record.get("title").and_then(serde_json::Value::as_str) {
                        title = Some(t.to_string());
                    }
                }
            }
            "message" => {
                message_count += 1;
                let msg = record.get("message");
                let role = msg
                    .and_then(|m| m.get("role"))
                    .and_then(serde_json::Value::as_str);
                let text = msg.and_then(|m| m.get("content")).and_then(message_text);
                match role {
                    Some("user") => {
                        if let Some(t) = text {
                            latest_user_prompt = Some(t);
                        }
                    }
                    Some("assistant") => {
                        if let Some(t) = text {
                            latest_assistant = Some(t);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Fallback identifiers from the file name `<ts>_<id>.jsonl`.
    let (basename_ts, basename_id) = split_basename(basename);
    session_id.get_or_insert_with(|| basename_id.to_string());
    if created_at.is_none() {
        created_at = basename_ts.and_then(parse_iso_epoch_seconds);
    }

    // Fallback for sessions whose records carry no timestamps.
    let mtime = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    updated_at = updated_at.max(mtime);

    Some(PersistentSession {
        session_id: session_id?,
        title,
        cwd,
        created_at,
        updated_at,
        message_count,
        latest_user_prompt: latest_user_prompt.map(|text| {
            let stripped = strip_leading_tag_lines(&text);
            cap_chars(stripped, MAX_USER_PROMPT_CHARS)
        }),
        latest_assistant_preview: latest_assistant
            .and_then(|text| assistant_preview(&text)),
    })
}

/// Parse a record `timestamp` field: either an ISO-8601 string or a millisecond
/// epoch number; both appear in real transcripts.
fn parse_timestamp(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().map(|ms| ms / 1000),
        serde_json::Value::String(s) => parse_iso_epoch_seconds(s),
        _ => None,
    }
}

/// Parse an ISO-8601 timestamp (with or without milliseconds, `Z` suffix or
/// `±HH:MM` offset) to epoch seconds. The basename creation format
/// `YYYY-MM-DDTHH-MM-SS-msZ` (dashes instead of colons) is accepted too.
fn parse_iso_epoch_seconds(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let digit = |b: u8| (b as char).to_digit(10);
    let two = |b0: u8, b1: u8| -> Option<u32> { Some(digit(b0)? * 10 + digit(b1)?) };
    let year = (digit(bytes[0])? as i64) * 1000
        + (digit(bytes[1])? as i64) * 100
        + (digit(bytes[2])? as i64) * 10
        + (digit(bytes[3])? as i64);
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let month = two(bytes[5], bytes[6])?;
    let day = two(bytes[8], bytes[9])?;
    let hour = two(bytes[11], bytes[12])?;
    let minute = two(bytes[14], bytes[15])?;
    let second = two(bytes[17], bytes[18])?;
    let mut offset_secs: i64 = 0;

    let mut i = 19;
    // Optional fraction: `.123` or `-123` (the basename form). A `-` here is
    // only a fraction separator when a zone marker follows the digits;
    // `2026-…T09:00:00-01:00` starts a timezone offset instead.
    if bytes.get(i) == Some(&b'.') || bytes.get(i) == Some(&b'-') {
        i += 1;
        while bytes.get(i).is_some_and(|b| b.is_ascii_digit()) {
            i += 1;
        }
        if !matches!(bytes.get(i), Some(&b'Z') | Some(&b'z') | None) {
            i = 19;
        }
    }
    // Optional zone: `Z` or `±HH:MM` / `±HHMM`.
    match bytes.get(i) {
        Some(&b'Z') | None => {}
        Some(&b'+') | Some(&b'-') => {
            let sign: i64 = if bytes[i] == b'-' { -1 } else { 1 };
            i += 1;
            let off_hour = two(*bytes.get(i)?, *bytes.get(i + 1)?)? as i64;
            let off_min = match bytes.get(i + 2) {
                Some(&b':') => two(*bytes.get(i + 3)?, *bytes.get(i + 4)?)? as i64,
                _ => two(*bytes.get(i + 2)?, *bytes.get(i + 3)?)? as i64,
            };
            offset_secs = sign * (off_hour * 3600 + off_min * 60);
        }
        _ => {}
    }

    let days = days_from_civil(year, month, day);
    let epoch = days * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64 - offset_secs;
    u64::try_from(epoch).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Extract the text of a message `content`: either a plain string or an array
/// of parts (text parts joined with newlines).
fn message_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let texts: Vec<String> = parts
                .iter()
                .filter_map(|part| {
                    let is_text = part.get("type").and_then(serde_json::Value::as_str)
                        == Some("text");
                    is_text
                        .then(|| part.get("text").and_then(serde_json::Value::as_str))
                        .flatten()
                        .map(String::from)
                })
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

/// Strip leading lines that are literal tags (`` `<...>` ``), e.g. an
/// `<attachment>` or `<environment_context>` marker before the real prompt.
fn strip_leading_tag_lines(text: &str) -> &str {
    let mut start = 0;
    loop {
        let rest = &text[start..];
        let line_end = rest.find('\n').unwrap_or(rest.len());
        let line = rest[..line_end].trim();
        if line.starts_with('<') && line.ends_with('>') {
            if line_end == rest.len() {
                return "";
            }
            start += line_end + 1;
        } else {
            return rest;
        }
    }
}

/// First line of an assistant message that reads as prose: non-empty, not
/// indented, not a list item, code fence, quote, heading, or table row.
fn assistant_preview(text: &str) -> Option<String> {
    text.lines()
        .find(|line| prose_line(line))
        .map(|line| cap_chars(line.trim(), MAX_ASSISTANT_PREVIEW_CHARS))
        .filter(|s| !s.is_empty())
}

fn prose_line(line: &str) -> bool {
    if line.trim().is_empty() {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return false;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        return false;
    }
    let first = trimmed.chars().next().unwrap();
    if matches!(first, '-' | '*' | '+' | '>' | '#' | '|') {
        return false;
    }
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = trimmed[digits..].trim_start();
        if rest.starts_with('.') || rest.starts_with(')') {
            return false;
        }
    }
    true
}

/// Truncate to `max` characters (not bytes).
fn cap_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// True when the `*.jsonl` file name matches the session pattern
/// `<creation-timestamp>_<session-id>.jsonl`.
fn matches_session_filename(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !name.ends_with(".jsonl") {
        return false;
    }
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    let Some(idx) = stem.find('_') else {
        return false;
    };
    let ts = &stem[..idx];
    let b = ts.as_bytes();
    if b.len() < 19 || b[10] != b'T' {
        return false;
    }
    let digit = |b: u8| b.is_ascii_digit();
    (0..4).all(|i| digit(b[i]))
        && b[4] == b'-'
        && digit(b[5])
        && digit(b[6])
        && b[7] == b'-'
        && digit(b[8])
        && digit(b[9])
        && digit(b[11])
        && digit(b[12])
        && b[13] == b'-'
        && digit(b[14])
        && digit(b[15])
        && b[16] == b'-'
        && digit(b[17])
        && digit(b[18])
}

/// Split `<ts>_<id>` into its creation timestamp and session id parts.
fn split_basename(basename: &str) -> (Option<&str>, &str) {
    match basename.find('_') {
        Some(idx) => (Some(&basename[..idx]), &basename[idx + 1..]),
        None => (None, basename),
    }
}

/// Parse a session jsonl path out of one line of a terminal-session record:
/// either a raw path string or a JSON object with a field naming the file.
fn session_path_from_line(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return json_session_path(&value);
    }
    looks_like_session_path(trimmed).then(|| PathBuf::from(trimmed))
}

/// Recursively find the first string that names a session transcript.
fn json_session_path(value: &serde_json::Value) -> Option<PathBuf> {
    match value {
        serde_json::Value::String(s) if looks_like_session_path(s) => Some(PathBuf::from(s)),
        serde_json::Value::Object(map) => {
            // Prefer obviously named fields, then any other value.
            for key in ["file", "sessionFile", "path", "session", "transcript"] {
                if let Some(hit) = map.get(key).and_then(json_session_path) {
                    return Some(hit);
                }
            }
            map.values().find_map(json_session_path)
        }
        serde_json::Value::Array(items) => items.iter().find_map(json_session_path),
        _ => None,
    }
}

fn looks_like_session_path(s: &str) -> bool {
    s.ends_with(".jsonl")
        && s.contains("/sessions/")
        && s.rsplit('/').next().is_some_and(|name| {
            matches_session_filename(Path::new(name))
        })
}

/// Files under `<omp_home>/../run/daemons/*/clients/*.json` describe live
/// clients with `{"pid": number, "projectDir": string}`. Returns `(pid,
/// projectDir)` pairs. Empty when the directory is missing.
pub fn running_clients(omp_home: &Path) -> Vec<(u32, String)> {
    let Some(parent) = omp_home.parent() else {
        return Vec::new();
    };
    let daemons_dir = parent.join("run").join("daemons");
    let Ok(daemons) = fs::read_dir(&daemons_dir) else {
        return Vec::new();
    };

    let mut clients: Vec<(u32, String)> = Vec::new();
    for daemon in daemons.flatten() {
        if !daemon.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let clients_dir = daemon.path().join("clients");
        let Ok(entries) = fs::read_dir(&clients_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.extension().is_some_and(|e| e == "json") {
                continue;
            }
            let Some(value) = fs::read_to_string(&path)
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            else {
                warn!("skipping unreadable daemon client file: {}", path.display());
                continue;
            };
            let pid = value.get("pid").and_then(|p| {
                p.as_u64()
                    .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
                    .and_then(|p| u32::try_from(p).ok())
            });
            let project_dir = value.get("projectDir").and_then(serde_json::Value::as_str);
            if let (Some(pid), Some(project_dir)) = (pid, project_dir) {
                clients.push((pid, project_dir.to_string()));
            }
        }
    }
    clients
}

/// TTY/tmux records under `<omp_home>/terminal-sessions/*`: each line is a
/// transcript path directly, or the last record is JSON naming the active
/// session file. Returns the distinct session jsonl paths, in file order.
pub fn active_session_files(omp_home: &Path) -> Vec<PathBuf> {
    let dir = omp_home.join("terminal-sessions");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut active: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in content.lines() {
            if let Some(path) = session_path_from_line(line) {
                if seen.insert(path.clone()) {
                    active.push(path);
                }
            }
        }
    }
    active
}

/// Unix: `true` when `kill(pid, 0)` succeeds (the process exists). Non-Unix:
/// always `true`.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    // `kill(-1, 0)` targets the caller's whole process group and would report
    // success, so reject pids outside the `pid_t` range up front.
    if pid == 0 || pid as u64 > i32::MAX as u64 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    /// Temp directory that removes itself on drop, even when a test panics.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "omp-ntfy-sessions-test-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Write a fixture file and pin its mtime so `updated_at` is deterministic.
    fn write_fixture(path: &Path, content: &str, mtime_secs: u64) {
        fs::write(path, content).unwrap();
        let mtime = UNIX_EPOCH + Duration::from_secs(mtime_secs);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    /// Full OMP layout under a fresh temp dir: `<base>/agent` (with sessions +
    /// terminal-sessions), `<base>/run/daemons/*/clients`.
    fn fixture_home() -> (TempDir, PathBuf) {
        let base = TempDir::new();
        let agent = base.path().join("agent");
        fs::create_dir_all(agent.join("sessions").join("proj-a")).unwrap();
        fs::create_dir_all(agent.join("terminal-sessions")).unwrap();
        (base, agent)
    }

    const TS_A: u64 = 1788256800; // 2026-09-01T10:00:00Z
    const TS_B: u64 = 1788256860; // 2026-09-01T10:01:00Z
    const TS_C: u64 = 1788256920; // 2026-09-01T10:02:00Z

    #[test]
    fn scan_sessions_parses_records_and_strips_tags() {
        let (_base, agent) = fixture_home();
        let dir = agent.join("sessions").join("proj-a");

        let file_a = dir.join("2026-09-01T10-00-00-000Z_aaa111.jsonl");
        write_fixture(
            &file_a,
            concat!(
                r#"{"type":"title","v":1,"title":"Header Title","updatedAt":"2026-09-01T10:00:00.000Z"}"#, "\n",
                r#"{"type":"session","version":3,"id":"aaa111","timestamp":"2026-09-01T10:00:00.000Z","cwd":"/tmp/proj"}"#, "\n",
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-09-01T10:01:00.000Z","message":{"role":"user","content":"hello there"}}"#, "\n",
                r#"{"type":"message","id":"m2","parentId":null,"timestamp":"2026-09-01T10:01:30.000Z","message":{"role":"assistant","content":[{"type":"text","text":"First line prose.\n\n- a list item\n```\nfence\n```\nSecond line prose."}]}}"#, "\n",
                r#"{"type":"message","id":"m3","parentId":null,"timestamp":"2026-09-01T10:01:45.000Z","message":{"role":"user","content":"<environment_context>\nWhich file format does OMP use for transcripts?"}}"#, "\n",
                r#"{"type":"title_change","id":"t1","parentId":"m2","timestamp":"2026-09-01T10:01:50.000Z","title":"Auto Title","source":"auto"}"#, "\n",
                r#"{"type":"message","id":"m4","parentId":null,"timestamp":"2026-09-01T10:02:00.000Z","message":{"role":"assistant","content":"Short answer.\n- item one\n- item two"}}"#, "\n",
            ),
            TS_A,
        );

        // Only a header record + malformed extras in a second session.
        let file_b = dir.join("2026-09-01T10-00-00-000Z_bbb222.jsonl");
        write_fixture(
            &file_b,
            concat!(
                r#"{"type":"title","v":1,"title":"Lonely Session","updatedAt":"2026-09-01T10:00:00.000Z"}"#, "\n",
                r#"{"type":"session","version":3,"id":"bbb222","timestamp":"2026-09-01T10:00:00.000Z","cwd":"/tmp/other"}"#, "\n",
                // broken trailing line, as if a live process was mid-append
                r#"{"type":"message","id":"x1","parentId":null,"timestamp":"2026-09-01T10:00:30.000Z","message":{"role":"user","content":"partial"#, "\n",
            ),
            TS_A,
        );

        // Files that must be ignored: bad names, wrong extension, no session dir.
        fs::write(dir.join("README.txt"), "nope").unwrap();
        fs::write(dir.join("garbage-id.jsonl"), "{}").unwrap();
        fs::write(agent.join("sessions").join("top-level.jsonl"), "{}").unwrap();

        let sessions = scan_sessions(&agent);
        assert_eq!(sessions.len(), 2, "only the two well-named sessions are scanned");

        let first = sessions.iter().find(|s| s.session_id == "aaa111").unwrap();
        assert_eq!(first.title.as_deref(), Some("Auto Title"), "title_change wins over header");
        assert_eq!(first.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(first.created_at, Some(TS_A));
        assert_eq!(first.updated_at, TS_C, "max record timestamp wins over header/mtime");
        assert_eq!(first.message_count, 4);
        assert_eq!(
            first.latest_user_prompt.as_deref(),
            Some("Which file format does OMP use for transcripts?"),
            "leading <environment_context> tag is stripped"
        );
        assert_eq!(
            first.latest_assistant_preview.as_deref(),
            Some("Short answer."),
            "first non-list prose line of latest assistant text"
        );

        let second = sessions.iter().find(|s| s.session_id == "bbb222").unwrap();
        assert_eq!(second.title.as_deref(), Some("Lonely Session"));
        assert_eq!(second.cwd.as_deref(), Some("/tmp/other"));
        assert_eq!(second.created_at, Some(TS_A));
        assert_eq!(second.updated_at, TS_A, "falls back to pinned file mtime");
        assert_eq!(second.message_count, 0, "broken trailing line is skipped");
        assert!(second.latest_user_prompt.is_none());
        assert!(second.latest_assistant_preview.is_none());

        // Newest (by updated_at) session sorts first.
        assert_eq!(sessions[0].session_id, "aaa111");
    }

    #[test]
    fn truncates_prompt_and_preview_to_caps() {
        let (_base, agent) = fixture_home();
        let dir = agent.join("sessions").join("proj-a");
        let long_prompt = "p".repeat(700);
        let long_answer = "a".repeat(1200);

        let file = dir.join("2026-09-01T10-00-00-000Z_ccc333.jsonl");
        write_fixture(
            &file,
            &format!(
                "{}{}{}{}{}{}",
                r#"{"type":"session","version":3,"id":"ccc333","timestamp":"2026-09-01T10:00:00.000Z","cwd":"/tmp/cc"}"#,
                "\n",
                format!(
                    r#"{{"type":"message","id":"m1","timestamp":"2026-09-01T10:01:00.000Z","message":{{"role":"user","content":"{long_prompt}"}}}}"#
                ),
                "\n",
                format!(
                    r#"{{"type":"message","id":"m2","timestamp":"2026-09-01T10:02:00.000Z","message":{{"role":"assistant","content":"{long_answer}"}}}}"#
                ),
                "\n",
            ),
            TS_A,
        );

        let sessions = scan_sessions(&agent);
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.latest_user_prompt.as_ref().unwrap().chars().count(), 500);
        assert_eq!(session.latest_assistant_preview.as_ref().unwrap().chars().count(), 1000);
    }

    #[test]
    fn running_clients_and_active_sessions() {
        let (base, agent) = fixture_home();

        // Missing daemon dirs -> empty, never an error.
        assert!(running_clients(&agent).is_empty());

        // A real daemon client layout.
        let clients = base
            .path()
            .join("run")
            .join("daemons")
            .join("deadbeef")
            .join("clients");
        fs::create_dir_all(&clients).unwrap();
        fs::write(clients.join("111-abc.json"), r#"{"pid":111,"id":"111-abc","projectDir":"/home/u/proj"}"#)
            .unwrap();
        fs::write(clients.join("222-def.json"), r#"{"pid":222,"projectDir":"/home/u/two"}"#).unwrap();
        fs::write(clients.join("bogus.json"), "not json at all").unwrap();
        fs::write(clients.join("notes.txt"), r#"{"pid":3,"projectDir":"/tmp"}"#).unwrap();

        let clients = running_clients(&agent);
        assert_eq!(clients.len(), 2, "only parseable *.json client records count");
        assert!(clients.contains(&(111, "/home/u/proj".to_string())));
        assert!(clients.contains(&(222, "/home/u/two".to_string())));

        // Terminal-session records: plain path lines plus a "fresh" marker.
        let tty = agent.join("terminal-sessions").join("pts-0");
        fs::write(
            &tty,
            concat!(
                "/home/u/proj\n",
                "/home/u/.omp/agent/sessions/-proj/2026-09-01T10-00-00-000Z_aaa111.jsonl\n",
                "fresh\n",
            ),
        )
        .unwrap();
        // JSON form with a field naming the session file.
        let tmux = agent.join("terminal-sessions").join("tmux-1");
        fs::write(
            &tmux,
            concat!(
                "/home/u/other\n",
                r#"{"pid":99,"sessionFile":"/home/u/.omp/agent/sessions/-proj/2026-09-01T10-00-00-000Z_bbb222.jsonl"}"#,
                "\n",
            ),
        )
        .unwrap();
        // Not a session path; must be ignored.
        let junk = agent.join("terminal-sessions").join("pts-9");
        fs::write(&junk, "/home/u/.omp/something-else\n").unwrap();

        let active = active_session_files(&agent);
        assert_eq!(active.len(), 2, "distinct session paths, JSON and plain forms");
        assert!(active.contains(&PathBuf::from(
            "/home/u/.omp/agent/sessions/-proj/2026-09-01T10-00-00-000Z_aaa111.jsonl"
        )));
        assert!(active.contains(&PathBuf::from(
            "/home/u/.omp/agent/sessions/-proj/2026-09-01T10-00-00-000Z_bbb222.jsonl"
        )));
    }

    #[test]
    fn pid_aliveness() {
        assert!(is_pid_alive(std::process::id()), "our own process is alive");
        // Out of pid_t range: must not be treated as "signal the whole group".
        assert!(!is_pid_alive(u32::MAX));
        assert!(!is_pid_alive(0));
    }

    #[test]
    fn timestamp_parsing() {
        assert_eq!(parse_iso_epoch_seconds("2026-09-01T10:00:00.000Z"), Some(TS_A));
        assert_eq!(parse_iso_epoch_seconds("2026-09-01T10:00:00Z"), Some(TS_A));
        assert_eq!(parse_iso_epoch_seconds("2026-09-01T10-00-00-000Z"), Some(TS_A));
        assert_eq!(parse_iso_epoch_seconds("2026-09-01T10:00:00+02:00"), Some(TS_A - 7200));
        assert_eq!(parse_iso_epoch_seconds("2026-09-01T09:00:00-01:00"), Some(TS_A));
        assert_eq!(parse_iso_epoch_seconds("junk"), None);
        assert!(parse_timestamp(&serde_json::json!(1788256800123u64)) == Some(TS_A));
    }
}