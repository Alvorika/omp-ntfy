//! Read-only aggregation over OMP's stats SQLite database (`~/.omp/stats.db`).
//!
//! The OMP process may be writing to the same database concurrently, so the
//! connection is opened strictly read-only with a short busy timeout, and any
//! error (missing file, busy, bad schema) surfaces as `None` with a log line.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

/// Aggregate counters over every LLM request recorded in `messages`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StatSummary {
    pub requests: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cost_usd: f64,
    pub subagent_requests: u64,
    /// Epoch seconds of the newest `messages` row.
    pub last_record_ts: Option<u64>,
}

const CACHE_TTL: Duration = Duration::from_secs(60);

/// Cached aggregator over a single stats database path.
pub struct StatsSource {
    path: PathBuf,
    cached: Option<(Instant, StatSummary)>,
}

impl StatsSource {
    /// `path` is the stats.db path. Does not open the DB yet.
    pub fn new(path: PathBuf) -> Self {
        Self { path, cached: None }
    }

    /// Returns the cached summary if queried within 60s; otherwise re-queries.
    /// Returns `None` on any error (missing file, bad table, busy) and logs a
    /// warn; never panics.
    pub fn summary(&mut self) -> Option<StatSummary> {
        if let Some((at, summary)) = &self.cached {
            if at.elapsed() < CACHE_TTL {
                return Some(summary.clone());
            }
        }
        match self.query() {
            Ok(summary) => {
                self.cached = Some((Instant::now(), summary.clone()));
                Some(summary)
            }
            Err(err) => {
                tracing::warn!(error = %err, path = %self.path.display(), "stats query failed");
                None
            }
        }
    }

    fn query(&self) -> anyhow::Result<StatSummary> {
        // Read-only URI open: WAL databases need a buildable -shm for a
        // plain read-only connection, which is not guaranteed under systemd.
        // immutable=1 snapshots the main DB file (aggregate queries only;
        // a handful of yet-to-be-checkpointed rows are irrelevant). Reject
        // URI filenames in the path to keep the path we log clean.
        let path_str = self.path.to_string_lossy();
        let uri = format!("file:{path_str}?mode=ro&immutable=1");
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(Duration::from_millis(100))?;
        let summary = conn.query_row(
            "SELECT COUNT(*), \
                    COALESCE(SUM(input_tokens),0), \
                    COALESCE(SUM(output_tokens),0), \
                    COALESCE(SUM(cache_read_tokens),0), \
                    COALESCE(SUM(cost_total),0), \
                    COALESCE(SUM(CASE WHEN agent_type='subagent' THEN 1 ELSE 0 END),0), \
                    MAX(timestamp) \
             FROM messages",
            [],
            |row| {
                Ok(StatSummary {
                    requests: row.get::<_, i64>(0)? as u64,
                    tokens_in: row.get::<_, i64>(1)? as u64,
                    tokens_out: row.get::<_, i64>(2)? as u64,
                    cache_read: row.get::<_, i64>(3)? as u64,
                    cost_usd: row.get::<_, f64>(4)?,
                    subagent_requests: row.get::<_, i64>(5)? as u64,
                    // timestamps are stored in ms; report seconds.
                    last_record_ts: row
                        .get::<_, Option<i64>>(6)?
                        .map(|ms| ms as u64 / 1000),
                })
            },
        )?;
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "omp-ntfy-stats-{tag}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn seed_db(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                session_file TEXT NOT NULL,
                entry_id TEXT NOT NULL,
                folder TEXT NOT NULL,
                model TEXT NOT NULL,
                provider TEXT NOT NULL,
                api TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                duration INTEGER,
                ttft INTEGER,
                stop_reason TEXT NOT NULL,
                error_message TEXT,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cache_write_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                premium_requests REAL NOT NULL,
                cost_input REAL NOT NULL,
                cost_output REAL NOT NULL,
                cost_cache_read REAL NOT NULL,
                cost_cache_write REAL NOT NULL,
                cost_total REAL NOT NULL,
                cost_no_cache_input REAL,
                agent_type TEXT NOT NULL DEFAULT 'main'
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (session_file, entry_id, folder, model, provider, api, timestamp, stop_reason,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_tokens,
              premium_requests, cost_input, cost_output, cost_cache_read, cost_cache_write,
              cost_total, agent_type)
             VALUES ('a', 'e1', 'f', 'm', 'p', 'openai-completions', 1000000, 'done',
              100, 50, 10, 0, 160, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 'main')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (session_file, entry_id, folder, model, provider, api, timestamp, stop_reason,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_tokens,
              premium_requests, cost_input, cost_output, cost_cache_read, cost_cache_write,
              cost_total, agent_type)
             VALUES ('b', 'e2', 'f', 'm', 'p', 'openai-completions', 2000000, 'done',
              200, 100, 20, 0, 320, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 'subagent')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages
             (session_file, entry_id, folder, model, provider, api, timestamp, stop_reason,
              input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_tokens,
              premium_requests, cost_input, cost_output, cost_cache_read, cost_cache_write,
              cost_total, agent_type)
             VALUES ('c', 'e3', 'f', 'm', 'p', 'openai-completions', 3000000, 'done',
              300, 150, 30, 0, 480, 0.0, 0.0, 0.0, 0.0, 0.0, 0.25, 'main')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn aggregates_seeded_rows() {
        let path = test_db_path("agg");
        seed_db(&path);

        let mut source = StatsSource::new(path.clone());
        let summary = source.summary().expect("summary should succeed");
        assert_eq!(summary.requests, 3);
        assert_eq!(summary.tokens_in, 600);
        assert_eq!(summary.tokens_out, 300);
        assert_eq!(summary.cache_read, 60);
        assert_eq!(summary.cost_usd, 0.75);
        assert_eq!(summary.subagent_requests, 1);
        assert_eq!(summary.last_record_ts, Some(3000)); // 3_000_000 ms / 1000

        // Second call within TTL returns the cache (works even after the file
        // is gone, proving no re-query happened).
        std::fs::remove_file(&path).unwrap();
        let cached = source.summary().expect("cached summary should succeed");
        assert_eq!(cached.requests, 3);
        assert_eq!(cached.last_record_ts, Some(3000));
    }

    #[test]
    fn missing_file_returns_none() {
        let path = test_db_path("missing");
        let mut source = StatsSource::new(path);
        assert!(source.summary().is_none());
    }
}