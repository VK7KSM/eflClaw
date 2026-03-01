//! Chat log indexing — SQLite summaries + FTS5 + embedding search.
//!
//! Adds **independent** tables to the existing `brain.db` (never touches `memories`).
//! Used by:
//! - Heartbeat worker: periodic background summarisation
//! - `SearchChatLogTool`: on-demand cross-user queries
//! - System prompt builder: inject cross-user context for the owner

use anyhow::Result;
use chrono::Local;
use rusqlite::{params, Connection};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Row from `chat_summaries` table.
#[derive(Debug, Clone)]
pub struct ChatSummaryRow {
    pub id: i64,
    pub channel: String,
    pub chat_id: String,
    pub chat_name: String,
    pub date: String,
    pub summary: String,
    pub topics: Option<String>,
    pub msg_count: i64,
}

/// Chat index backed by SQLite.
pub struct ChatIndex {
    conn: Arc<Mutex<Connection>>,
    #[allow(dead_code)]
    db_path: PathBuf,
}

// ── DB watchdog thresholds ──────────────────────────────────────
const WATCHDOG_ROW_THRESHOLD: i64 = 100_000;
const WATCHDOG_SIZE_MB_THRESHOLD: u64 = 200;

impl ChatIndex {
    /// Open (or create) the chat index tables inside `brain.db`.
    pub fn open(workspace_dir: &Path) -> Result<Self> {
        let db_path = workspace_dir.join("memory").join("brain.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;

        // WAL mode + tuning (same as SqliteMemory)
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous  = NORMAL;
             PRAGMA temp_store   = MEMORY;",
        )?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            db_path,
        })
    }

    /// Create tables if they don't exist (safe to run repeatedly).
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chat_summaries (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                channel     TEXT NOT NULL DEFAULT 'telegram',
                chat_id     TEXT NOT NULL,
                chat_name   TEXT NOT NULL,
                date        TEXT NOT NULL,
                summary     TEXT NOT NULL,
                topics      TEXT,
                embedding   BLOB,
                msg_count   INTEGER DEFAULT 0,
                source_hash TEXT,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                UNIQUE(channel, chat_id, date)
            );
            CREATE INDEX IF NOT EXISTS idx_cs_chat ON chat_summaries(chat_id);
            CREATE INDEX IF NOT EXISTS idx_cs_date ON chat_summaries(date);
            CREATE INDEX IF NOT EXISTS idx_cs_name ON chat_summaries(chat_name);

            CREATE VIRTUAL TABLE IF NOT EXISTS chat_summaries_fts USING fts5(
                chat_name, summary, topics,
                content=chat_summaries, content_rowid=id
            );

            -- Keep FTS in sync
            CREATE TRIGGER IF NOT EXISTS cs_ai AFTER INSERT ON chat_summaries BEGIN
                INSERT INTO chat_summaries_fts(rowid, chat_name, summary, topics)
                VALUES (new.id, new.chat_name, new.summary, new.topics);
            END;
            CREATE TRIGGER IF NOT EXISTS cs_ad AFTER DELETE ON chat_summaries BEGIN
                INSERT INTO chat_summaries_fts(chat_summaries_fts, rowid, chat_name, summary, topics)
                VALUES ('delete', old.id, old.chat_name, old.summary, old.topics);
            END;
            CREATE TRIGGER IF NOT EXISTS cs_au AFTER UPDATE ON chat_summaries BEGIN
                INSERT INTO chat_summaries_fts(chat_summaries_fts, rowid, chat_name, summary, topics)
                VALUES ('delete', old.id, old.chat_name, old.summary, old.topics);
                INSERT INTO chat_summaries_fts(rowid, chat_name, summary, topics)
                VALUES (new.id, new.chat_name, new.summary, new.topics);
            END;",
        )?;
        Ok(())
    }

    /// Upsert a daily summary (insert or update if already exists).
    pub fn upsert_summary(
        &self,
        channel: &str,
        chat_id: &str,
        chat_name: &str,
        date: &str,
        summary: &str,
        topics: Option<&str>,
        embedding: Option<&[f32]>,
        msg_count: i64,
        source_hash: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = Local::now().to_rfc3339();
        let emb_bytes = embedding.map(vec_to_bytes);

        conn.execute(
            "INSERT INTO chat_summaries
                (channel, chat_id, chat_name, date, summary, topics, embedding, msg_count, source_hash, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(channel, chat_id, date) DO UPDATE SET
                chat_name   = excluded.chat_name,
                summary     = excluded.summary,
                topics      = excluded.topics,
                embedding   = excluded.embedding,
                msg_count   = excluded.msg_count,
                source_hash = excluded.source_hash,
                updated_at  = excluded.updated_at",
            params![channel, chat_id, chat_name, date, summary, topics, emb_bytes, msg_count, source_hash, now, now],
        )?;
        Ok(())
    }

    /// Get the source hash for a specific (channel, chat_id, date) entry.
    /// Returns None if no entry exists yet.
    pub fn get_source_hash(
        &self,
        channel: &str,
        chat_id: &str,
        date: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT source_hash FROM chat_summaries WHERE channel = ?1 AND chat_id = ?2 AND date = ?3",
            params![channel, chat_id, date],
            |row| row.get::<_, Option<String>>(0),
        );
        match result {
            Ok(hash) => Ok(hash),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Full-text search across all summaries (BM25 ranked).
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<ChatSummaryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT cs.id, cs.channel, cs.chat_id, cs.chat_name, cs.date, cs.summary, cs.topics, cs.msg_count
             FROM chat_summaries_fts fts
             JOIN chat_summaries cs ON cs.id = fts.rowid
             WHERE chat_summaries_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![query, limit as i64], |row| {
                Ok(ChatSummaryRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    chat_id: row.get(2)?,
                    chat_name: row.get(3)?,
                    date: row.get(4)?,
                    summary: row.get(5)?,
                    topics: row.get(6)?,
                    msg_count: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Get summaries for a specific user, ordered by date descending.
    pub fn get_user_summaries(&self, chat_name: &str, limit: usize) -> Result<Vec<ChatSummaryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel, chat_id, chat_name, date, summary, topics, msg_count
             FROM chat_summaries
             WHERE chat_name = ?1
             ORDER BY date DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![chat_name, limit as i64], |row| {
                Ok(ChatSummaryRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    chat_id: row.get(2)?,
                    chat_name: row.get(3)?,
                    date: row.get(4)?,
                    summary: row.get(5)?,
                    topics: row.get(6)?,
                    msg_count: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Get recent summaries across ALL users (for cross-user awareness injection).
    pub fn get_recent_cross_user_summaries(
        &self,
        exclude_user: &str,
        limit: usize,
    ) -> Result<Vec<ChatSummaryRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, channel, chat_id, chat_name, date, summary, topics, msg_count
             FROM chat_summaries
             WHERE chat_name != ?1
             ORDER BY date DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![exclude_user, limit as i64], |row| {
                Ok(ChatSummaryRow {
                    id: row.get(0)?,
                    channel: row.get(1)?,
                    chat_id: row.get(2)?,
                    chat_name: row.get(3)?,
                    date: row.get(4)?,
                    summary: row.get(5)?,
                    topics: row.get(6)?,
                    msg_count: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// DB watchdog: check if thresholds are exceeded.
    /// Returns Some(message) if action needed, None if OK.
    pub fn watchdog_check(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();

        // Row count
        let row_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM chat_summaries", [], |row| row.get(0))?;

        // DB file size
        let db_size_bytes: i64 = conn.query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get(0),
        )?;
        let db_size_mb = db_size_bytes as u64 / (1024 * 1024);

        if row_count > WATCHDOG_ROW_THRESHOLD || db_size_mb > WATCHDOG_SIZE_MB_THRESHOLD {
            Ok(Some(format!(
                "📊 聊天记录数据库已有 {} 条索引记录 ({} MB)，建议清理旧数据。\n\
                 使用命令：zeroclaw memory compact --before <日期>",
                row_count, db_size_mb
            )))
        } else {
            Ok(None)
        }
    }

    /// Total number of summary rows (for monitoring).
    pub fn summary_count(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM chat_summaries", [], |row| row.get(0))?;
        Ok(count)
    }
}

/// Compute hash of file content for change detection (non-cryptographic).
pub fn file_content_hash(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Convert f32 vector to bytes for SQLite BLOB storage.
fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_index(tmp: &TempDir) -> ChatIndex {
        ChatIndex::open(tmp.path()).unwrap()
    }

    #[test]
    fn upsert_and_query_summary() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        idx.upsert_summary(
            "telegram",
            "123",
            "Alice",
            "2026-02-26",
            "讨论了天气和晚饭",
            Some("天气,晚饭"),
            None,
            15,
            "hash1",
        )
        .unwrap();

        let rows = idx.get_user_summaries("Alice", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_name, "Alice");
        assert_eq!(rows[0].summary, "讨论了天气和晚饭");
        assert_eq!(rows[0].msg_count, 15);
    }

    #[test]
    fn upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        idx.upsert_summary(
            "telegram",
            "123",
            "Bob",
            "2026-02-26",
            "上午的对话",
            None,
            None,
            5,
            "hash1",
        )
        .unwrap();

        idx.upsert_summary(
            "telegram",
            "123",
            "Bob",
            "2026-02-26",
            "上午和下午的对话",
            Some("编程"),
            None,
            20,
            "hash2",
        )
        .unwrap();

        let rows = idx.get_user_summaries("Bob", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "上午和下午的对话");
        assert_eq!(rows[0].msg_count, 20);
    }

    #[test]
    fn source_hash_check() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        assert!(idx
            .get_source_hash("telegram", "123", "2026-02-26")
            .unwrap()
            .is_none());

        idx.upsert_summary(
            "telegram",
            "123",
            "Alice",
            "2026-02-26",
            "test",
            None,
            None,
            1,
            "abc123",
        )
        .unwrap();

        let hash = idx
            .get_source_hash("telegram", "123", "2026-02-26")
            .unwrap();
        assert_eq!(hash.as_deref(), Some("abc123"));
    }

    #[test]
    fn fts_search() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        idx.upsert_summary(
            "telegram",
            "1",
            "Alice",
            "2026-02-26",
            "discussed weather and dinner plans",
            Some("weather,dinner"),
            None,
            10,
            "h1",
        )
        .unwrap();

        idx.upsert_summary(
            "telegram",
            "2",
            "Bob",
            "2026-02-26",
            "talked about Python programming",
            Some("programming"),
            None,
            8,
            "h2",
        )
        .unwrap();

        let results = idx.search_fts("weather", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chat_name, "Alice");

        let results = idx.search_fts("programming", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chat_name, "Bob");
    }

    #[test]
    fn cross_user_summaries_excludes_self() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        idx.upsert_summary(
            "telegram",
            "1",
            "Alice",
            "2026-02-26",
            "hello",
            None,
            None,
            1,
            "h1",
        )
        .unwrap();
        idx.upsert_summary(
            "telegram",
            "2",
            "Bob",
            "2026-02-26",
            "world",
            None,
            None,
            1,
            "h2",
        )
        .unwrap();

        let results = idx.get_recent_cross_user_summaries("Alice", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chat_name, "Bob");
    }

    #[test]
    fn watchdog_below_threshold() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);
        assert!(idx.watchdog_check().unwrap().is_none());
    }

    #[test]
    fn summary_count() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);
        assert_eq!(idx.summary_count().unwrap(), 0);

        idx.upsert_summary("telegram", "1", "A", "2026-01-01", "s", None, None, 1, "h")
            .unwrap();
        assert_eq!(idx.summary_count().unwrap(), 1);
    }

    #[test]
    fn file_content_hash_deterministic() {
        let h1 = file_content_hash("hello world");
        let h2 = file_content_hash("hello world");
        assert_eq!(h1, h2);
        assert_ne!(h1, file_content_hash("different"));
    }
}
