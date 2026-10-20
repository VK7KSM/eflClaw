//! elfClaw 2026-10-20: lessons learned from corrections ("经验手册").
//!
//! The old self-improvement path stored the nightly chat summary's
//! "纠错" line as a `correction_<date>_<chat>` memory and put every such
//! memory into the prompt. Those lines described what went wrong ("用户纠正
//! 助手未查看后院监控") rather than what to do next time, arrived the next
//! day, overwrote each other per chat-day, and nobody checked them — so
//! being corrected changed nothing.
//!
//! A [`Lesson`] is a procedure instead: *when* it applies and *what to do*,
//! plus the evidence it came from. It starts as `proposed`; only the owner
//! turns it `active` (an inline button), and only active lessons reach the
//! prompt ([`lessons_for_prompt`]). Lessons are added, approved, rejected or
//! retired one at a time and never rewritten wholesale — the failure mode
//! the self-improvement literature calls context collapse.
//!
//! Storage only: nothing here calls a model or sends a message (see
//! `channels::lessons`).

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Lessons put into the prompt at most; newest first.
pub const MAX_IN_PROMPT: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Proposed,
    Active,
    Rejected,
    Retired,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Rejected => "rejected",
            Self::Retired => "retired",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "rejected" => Self::Rejected,
            "retired" => Self::Retired,
            _ => Self::Proposed,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    pub id: i64,
    pub status: Status,
    /// When the lesson applies, in the user's language.
    pub applies_when: String,
    /// What to do then.
    pub rule: String,
    /// What happened: the mistake and the user's correction.
    pub evidence: String,
    /// Who corrected (chat user name), for the owner's card.
    pub source: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct LessonStore {
    conn: Arc<Mutex<Connection>>,
}

fn db_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("memory").join("lessons.db")
}

impl LessonStore {
    pub fn open(workspace_dir: &Path) -> Result<Self> {
        let path = db_path(workspace_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS lessons (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 status       TEXT NOT NULL,
                 applies_when TEXT NOT NULL,
                 rule         TEXT NOT NULL,
                 evidence     TEXT NOT NULL,
                 source       TEXT NOT NULL,
                 created_at   TEXT NOT NULL,
                 decided_at   TEXT
             );
             CREATE TABLE IF NOT EXISTS meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// A new proposal, waiting for the owner.
    pub fn propose(
        &self,
        applies_when: &str,
        rule: &str,
        evidence: &str,
        source: &str,
    ) -> Result<Lesson> {
        let now = Utc::now();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO lessons (status, applies_when, rule, evidence, source, created_at)
             VALUES ('proposed', ?1, ?2, ?3, ?4, ?5)",
            params![applies_when, rule, evidence, source, now.to_rfc3339()],
        )?;
        Ok(Lesson {
            id: conn.last_insert_rowid(),
            status: Status::Proposed,
            applies_when: applies_when.to_string(),
            rule: rule.to_string(),
            evidence: evidence.to_string(),
            source: source.to_string(),
            created_at: now,
        })
    }

    pub fn get(&self, id: i64) -> Result<Option<Lesson>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, status, applies_when, rule, evidence, source, created_at
             FROM lessons WHERE id = ?1",
            [id],
            row_to_lesson,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Move a lesson on. Only these steps exist: a proposal is approved or
    /// rejected, an active lesson is retired. Returns the lesson as it is
    /// now, or `None` when the step does not apply (already decided, gone).
    pub fn decide(&self, id: i64, to: Status) -> Result<Option<Lesson>> {
        let from = match to {
            Status::Active | Status::Rejected => Status::Proposed,
            Status::Retired => Status::Active,
            Status::Proposed => return Ok(None),
        };
        let changed = self.conn.lock().execute(
            "UPDATE lessons SET status = ?1, decided_at = ?2 WHERE id = ?3 AND status = ?4",
            params![to.as_str(), Utc::now().to_rfc3339(), id, from.as_str()],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.get(id)
    }

    /// Lessons with this status, newest first.
    pub fn with_status(&self, status: Status, limit: usize) -> Result<Vec<Lesson>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, status, applies_when, rule, evidence, source, created_at
             FROM lessons WHERE status = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![status.as_str(), i64::try_from(limit).unwrap_or(i64::MAX)],
            row_to_lesson,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Has `source` had a lesson proposed since `since`? Keeps the nightly
    /// fallback from proposing again what was proposed at the time.
    pub fn proposed_since(&self, source: &str, since: DateTime<Utc>) -> Result<bool> {
        let n: i64 = self.conn.lock().query_row(
            "SELECT COUNT(*) FROM lessons WHERE source = ?1 AND created_at >= ?2",
            params![source, since.to_rfc3339()],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .lock()
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .map_err(Into::into)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

fn row_to_lesson(r: &rusqlite::Row<'_>) -> rusqlite::Result<Lesson> {
    let created: String = r.get(6)?;
    Ok(Lesson {
        id: r.get(0)?,
        status: Status::parse(&r.get::<_, String>(1)?),
        applies_when: r.get(2)?,
        rule: r.get(3)?,
        evidence: r.get(4)?,
        source: r.get(5)?,
        created_at: DateTime::parse_from_rfc3339(&created)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

/// The prompt section with every active lesson (newest first, capped), or
/// an empty string when there are none.
pub fn lessons_for_prompt(store: &LessonStore) -> String {
    let lessons = store
        .with_status(Status::Active, MAX_IN_PROMPT)
        .unwrap_or_default();
    render_for_prompt(&lessons)
}

fn render_for_prompt(lessons: &[Lesson]) -> String {
    if lessons.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## 已确认的经验\n\n以下是你以前被纠正后总结、并经主人确认的做法。遇到对应情况必须照做：\n\n",
    );
    for l in lessons {
        let _ = writeln!(out, "- [经验 #{}] 当{}：{}", l.id, l.applies_when, l.rule);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_approved_lessons_reach_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = LessonStore::open(dir.path()).unwrap();
        let a = store
            .propose(
                "问某件事何时发生",
                "先查事件时间线",
                "说错了时间",
                "zeroclaw_user",
            )
            .unwrap();
        let b = store
            .propose("问摄像头情况", "先列摄像头", "漏看一路", "zeroclaw_user")
            .unwrap();
        assert_eq!(lessons_for_prompt(&store), "");

        assert!(store.decide(a.id, Status::Active).unwrap().is_some());
        assert!(store.decide(b.id, Status::Rejected).unwrap().is_some());
        let prompt = lessons_for_prompt(&store);
        assert!(prompt.contains(&format!(
            "[经验 #{}] 当问某件事何时发生：先查事件时间线",
            a.id
        )));
        assert!(!prompt.contains("先列摄像头"));
    }

    #[test]
    fn decisions_only_move_forward() {
        let dir = tempfile::tempdir().unwrap();
        let store = LessonStore::open(dir.path()).unwrap();
        let a = store.propose("x", "y", "z", "u").unwrap();
        // A proposal cannot be retired, an active lesson cannot be rejected.
        assert!(store.decide(a.id, Status::Retired).unwrap().is_none());
        assert!(store.decide(a.id, Status::Active).unwrap().is_some());
        assert!(store.decide(a.id, Status::Active).unwrap().is_none());
        assert!(store.decide(a.id, Status::Rejected).unwrap().is_none());
        let retired = store.decide(a.id, Status::Retired).unwrap().unwrap();
        assert_eq!(retired.status, Status::Retired);
        assert_eq!(lessons_for_prompt(&store), "");
    }

    #[test]
    fn proposals_are_remembered_per_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = LessonStore::open(dir.path()).unwrap();
        let before = Utc::now() - chrono::Duration::minutes(1);
        assert!(!store.proposed_since("zeroclaw_user", before).unwrap());
        store.propose("x", "y", "z", "zeroclaw_user").unwrap();
        assert!(store.proposed_since("zeroclaw_user", before).unwrap());
        assert!(!store.proposed_since("other_user", before).unwrap());
    }
}
