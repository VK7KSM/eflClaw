// elfClaw: SQLite + JSONL log store for diagnostics
use super::types::LogEntry;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Persistent log store backed by SQLite (structured queries) + JSONL (append-only archive).
pub struct LogStore {
    db_path: PathBuf,
    jsonl_path: PathBuf,
    /// Guards JSONL file writes to avoid interleaving from concurrent tasks.
    jsonl_lock: Mutex<()>,
}

const DEFAULT_PRUNE_DAYS: u32 = 7;

impl LogStore {
    /// Initialise the log store. Creates `state/` directory, SQLite DB (WAL mode),
    /// and prunes entries older than `keep_days`.
    pub fn init(workspace_dir: &Path) -> Result<Self> {
        let state_dir = workspace_dir.join("state");
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("Failed to create state dir: {}", state_dir.display()))?;

        let db_path = state_dir.join("elfclaw-logs.db");
        let jsonl_path = state_dir.join("elfclaw-logs.jsonl");

        // Open DB and create schema
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open log DB: {}", db_path.display()))?;
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS elfclaw_logs (
                id        TEXT PRIMARY KEY,
                timestamp TEXT NOT NULL,
                level     TEXT NOT NULL,
                category  TEXT NOT NULL,
                component TEXT NOT NULL,
                message   TEXT NOT NULL,
                details   TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_logs_ts       ON elfclaw_logs(timestamp);
            CREATE INDEX IF NOT EXISTS idx_logs_level    ON elfclaw_logs(level);
            CREATE INDEX IF NOT EXISTS idx_logs_category ON elfclaw_logs(category);",
        )
        .context("Failed to initialise elfclaw_logs schema")?;

        // Prune old entries on startup
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(DEFAULT_PRUNE_DAYS));
        let cutoff_str = cutoff.to_rfc3339();
        let pruned = conn
            .execute(
                "DELETE FROM elfclaw_logs WHERE timestamp < ?1",
                params![cutoff_str],
            )
            .unwrap_or(0);
        if pruned > 0 {
            tracing::info!(
                "elfclaw_log: pruned {pruned} log entries older than {DEFAULT_PRUNE_DAYS} days"
            );
        }

        Ok(Self {
            db_path,
            jsonl_path,
            jsonl_lock: Mutex::new(()),
        })
    }

    /// Write a single log entry to both SQLite and JSONL.
    pub fn write(&self, entry: &LogEntry) {
        // SQLite INSERT (best-effort — never crash the main flow)
        if let Err(e) = self.write_sqlite(entry) {
            tracing::warn!("elfclaw_log: SQLite write failed: {e}");
        }

        // JSONL append (best-effort)
        if let Err(e) = self.write_jsonl(entry) {
            tracing::warn!("elfclaw_log: JSONL write failed: {e}");
        }
    }

    /// Query recent log entries with optional level, category, and time-range filters.
    /// `since_minutes`: if `Some(n)`, only return entries from the last n minutes.
    #[allow(dead_code)]
    pub fn query_recent(
        &self,
        limit: usize,
        level_filter: Option<&str>,
        category_filter: Option<&str>,
        since_minutes: Option<u64>,
    ) -> Result<Vec<LogEntry>> {
        let conn = Connection::open(&self.db_path)?;
        let mut sql = String::from(
            "SELECT id, timestamp, level, category, component, message, details
             FROM elfclaw_logs WHERE 1=1",
        );
        let mut bind_values: Vec<String> = Vec::new();

        if let Some(level) = level_filter {
            sql.push_str(&format!(" AND level = ?{}", bind_values.len() + 1));
            bind_values.push(level.to_string());
        }
        if let Some(cat) = category_filter {
            sql.push_str(&format!(" AND category = ?{}", bind_values.len() + 1));
            bind_values.push(cat.to_string());
        }
        // elfClaw: time-range filter — only logs newer than (now - since_minutes)
        if let Some(mins) = since_minutes {
            let cutoff = chrono::Utc::now() - chrono::Duration::minutes(mins as i64);
            sql.push_str(&format!(" AND timestamp >= ?{}", bind_values.len() + 1));
            bind_values.push(cutoff.to_rfc3339());
        }

        sql.push_str(&format!(
            " ORDER BY timestamp DESC LIMIT ?{}",
            bind_values.len() + 1
        ));
        let lim = i64::try_from(limit.max(1)).unwrap_or(100);
        bind_values.push(lim.to_string());

        let mut stmt = conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::types::ToSql> = bind_values
            .iter()
            .map(|v| v as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            let details_raw: Option<String> = row.get(6)?;
            let details = details_raw
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or(serde_json::Value::Null);
            Ok(LogEntry {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                level: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(2)?))
                    .unwrap_or(super::types::LogLevel::Info),
                category: serde_json::from_str(&format!("\"{}\"", row.get::<_, String>(3)?))
                    .unwrap_or(super::types::LogCategory::System),
                component: row.get(4)?,
                message: row.get(5)?,
                details,
            })
        })?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }

    fn write_sqlite(&self, entry: &LogEntry) -> Result<()> {
        let conn = Connection::open(&self.db_path)?;
        let details_json = serde_json::to_string(&entry.details).unwrap_or_default();
        conn.execute(
            "INSERT OR IGNORE INTO elfclaw_logs (id, timestamp, level, category, component, message, details)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.id,
                entry.timestamp,
                entry.level.as_str(),
                entry.category.as_str(),
                entry.component,
                entry.message,
                details_json,
            ],
        )?;
        Ok(())
    }

    fn write_jsonl(&self, entry: &LogEntry) -> Result<()> {
        let _guard = self.jsonl_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
    use super::*;

    #[test]
    fn init_creates_db_and_writes_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = LogStore::init(tmp.path()).unwrap();

        let entry = LogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: LogLevel::Info,
            category: LogCategory::System,
            component: "test".into(),
            message: "hello".into(),
            details: serde_json::json!({"foo": "bar"}),
        };
        store.write(&entry);

        let results = store.query_recent(10, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, entry.id);
        assert_eq!(results[0].component, "test");
    }

    #[test]
    fn query_filters_by_level_and_category() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = LogStore::init(tmp.path()).unwrap();

        for (i, level) in [LogLevel::Info, LogLevel::Error, LogLevel::Warn]
            .iter()
            .enumerate()
        {
            store.write(&LogEntry {
                id: format!("id-{i}"),
                timestamp: chrono::Utc::now().to_rfc3339(),
                level: *level,
                category: LogCategory::ToolCall,
                component: "test".into(),
                message: format!("msg {i}"),
                details: serde_json::Value::Null,
            });
        }

        let errors = store.query_recent(10, Some("error"), None, None).unwrap();
        assert_eq!(errors.len(), 1);

        let tools = store
            .query_recent(10, None, Some("tool_call"), None)
            .unwrap();
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn jsonl_file_created_on_write() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = LogStore::init(tmp.path()).unwrap();

        store.write(&LogEntry {
            id: "jsonl-test".into(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: LogLevel::Debug,
            category: LogCategory::System,
            component: "test".into(),
            message: "jsonl test".into(),
            details: serde_json::Value::Null,
        });

        let contents = std::fs::read_to_string(&store.jsonl_path).unwrap();
        assert!(contents.contains("jsonl-test"));
    }
}
