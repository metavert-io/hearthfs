use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value as JsonValue;

/// Append-only event log backed by SQLite.
/// Thread-safe via internal Mutex.
pub struct EventLog {
    conn: Mutex<Connection>,
}

/// Summary statistics from the event log.
#[derive(Debug, Clone)]
pub struct Stats {
    pub total_reads: i64,
    pub total_searches: i64,
    pub total_magic_reads: i64,
    pub total_index_events: i64,
    pub tokens_saved: i64,
    pub avg_read_ms: f64,
    pub avg_search_ms: f64,
}

impl EventLog {
    /// Open (or create) the event log database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%f', 'now')),
                category TEXT NOT NULL,
                event_type TEXT NOT NULL,
                path TEXT,
                details TEXT,
                tokens_saved INTEGER,
                duration_ms INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_events_category
                ON events(category, timestamp);
            CREATE INDEX IF NOT EXISTS idx_events_path
                ON events(path, timestamp);
            ",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record an event.
    pub fn record(
        &self,
        category: &str,
        event_type: &str,
        path: Option<&str>,
        details: Option<&JsonValue>,
        tokens_saved: Option<i64>,
        duration_ms: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO events (category, event_type, path, details, tokens_saved, duration_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                category,
                event_type,
                path,
                details.map(|d| d.to_string()),
                tokens_saved,
                duration_ms,
            ),
        )?;
        Ok(())
    }

    /// Get total token savings.
    pub fn total_tokens_saved(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(tokens_saved), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    /// Get total number of events by category.
    pub fn count_by_category(&self, category: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE category = ?1",
            [category],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Get summary statistics.
    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn.lock().unwrap();
        let total_reads: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE category = 'fs' AND event_type = 'read'",
            [],
            |row| row.get(0),
        )?;
        let total_searches: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE category = 'query'",
            [],
            |row| row.get(0),
        )?;
        let total_magic_reads: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE category = 'fs' AND event_type = 'magic_read'",
            [],
            |row| row.get(0),
        )?;
        let total_index_events: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE category = 'index'",
            [],
            |row| row.get(0),
        )?;
        let tokens_saved: i64 = conn.query_row(
            "SELECT COALESCE(SUM(tokens_saved), 0) FROM events",
            [],
            |row| row.get(0),
        )?;
        let avg_read_ms: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(duration_ms), 0) FROM events WHERE category = 'fs'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0.0);
        let avg_search_ms: f64 = conn
            .query_row(
                "SELECT COALESCE(AVG(duration_ms), 0) FROM events WHERE category = 'query'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0.0);

        Ok(Stats {
            total_reads,
            total_searches,
            total_magic_reads,
            total_index_events,
            tokens_saved,
            avg_read_ms,
            avg_search_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_event_log_roundtrip() {
        let log = EventLog::open(Path::new(":memory:")).unwrap();

        log.record("fs", "read", Some("/foo/bar.rs"), None, Some(500), Some(12))
            .unwrap();
        log.record(
            "query",
            "search",
            Some("/foo"),
            Some(&json!({"query": "auth"})),
            None,
            Some(45),
        )
        .unwrap();

        assert_eq!(log.total_tokens_saved().unwrap(), 500);
        assert_eq!(log.count_by_category("fs").unwrap(), 1);
        assert_eq!(log.count_by_category("query").unwrap(), 1);
    }

    #[test]
    fn test_stats() {
        let log = EventLog::open(Path::new(":memory:")).unwrap();
        log.record("fs", "read", Some("/a.rs"), None, None, Some(5))
            .unwrap();
        log.record("fs", "read", Some("/b.rs"), None, None, Some(15))
            .unwrap();
        log.record("fs", "magic_read", Some("/a.rs@tokens"), None, Some(100), Some(2))
            .unwrap();
        log.record("query", "search", None, None, None, Some(30))
            .unwrap();
        log.record("index", "file_indexed", Some("/a.rs"), None, None, Some(50))
            .unwrap();

        let stats = log.stats().unwrap();
        assert_eq!(stats.total_reads, 2);
        assert_eq!(stats.total_searches, 1);
        assert_eq!(stats.total_magic_reads, 1);
        assert_eq!(stats.total_index_events, 1);
        assert_eq!(stats.tokens_saved, 100);
    }
}
