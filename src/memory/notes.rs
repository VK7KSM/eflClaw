//! elfClaw 2026-09-23: structured notes/reminders ("记事").
//!
//! This is deliberately a separate, much simpler system from the rest of
//! `src/memory/` (the embedding/FTS-backed `Memory` trait implementations).
//! Per elfclaw.md §7, the old design sat between two bad extremes: either
//! the *entire* long-term memory file was stuffed into every prompt, or
//! recall relied on semantic top-5 search against a store that was mostly
//! full of auto-saved raw chat messages — so a plain "remember X" note
//! could easily rank below unrelated chatter and never surface again, and
//! nothing had a due date or a done/not-done state at all.
//!
//! A [`Note`] is a plain record: content, when it was created, an optional
//! due time, and whether it's done. [`open_notes_for_prompt`] is the only
//! thing that needs to touch the system prompt — it returns just the
//! *unfinished* notes, each dated, capped at a small limit, so the model
//! always sees "what's still outstanding" without the prompt growing
//! unbounded as notes pile up over months.
//!
//! A note with a `due_at` is also a reminder: the chat agent goes through
//! [`NoteStore::add`] (via the `note_add` tool), and firing at `due_at` is
//! handled entirely by a `JobType::Message` cron job (see
//! `cron::scheduler` and elfclaw.md §6.4) — not by anything in this file.
//! This module only owns *storage*; nothing here calls an LLM or sends a
//! message anywhere.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Note {
    pub id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub due_at: Option<DateTime<Utc>>,
    pub done: bool,
}

/// elfClaw: notes get their own small SQLite file, same pattern as
/// `cron::store`'s standalone `jobs.db` — kept separate from `brain.db`'s
/// embedding-heavy schema rather than added as more columns on `memories`,
/// since a note's shape (content, due date, done) has nothing to do with
/// vectors or FTS.
#[derive(Clone)]
pub struct NoteStore {
    conn: Arc<Mutex<Connection>>,
}

fn db_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("memory").join("notes.db")
}

impl NoteStore {
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
             CREATE TABLE IF NOT EXISTS notes (
                 id         TEXT PRIMARY KEY,
                 content    TEXT NOT NULL,
                 created_at TEXT NOT NULL,
                 due_at     TEXT,
                 done       INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_notes_done_due ON notes(done, due_at);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn add(&self, content: &str, due_at: Option<DateTime<Utc>>) -> Result<Note> {
        let note = Note {
            id: Uuid::new_v4().to_string(),
            content: content.to_string(),
            created_at: Utc::now(),
            due_at,
            done: false,
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO notes (id, content, created_at, due_at, done) VALUES (?1, ?2, ?3, ?4, 0)",
            params![
                note.id,
                note.content,
                note.created_at.to_rfc3339(),
                note.due_at.map(|d| d.to_rfc3339()),
            ],
        )
        .context("inserting note")?;
        Ok(note)
    }

    pub fn mark_done(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute("UPDATE notes SET done = 1 WHERE id = ?1", params![id])
            .context("marking note done")?;
        Ok(changed > 0)
    }

    pub fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute("DELETE FROM notes WHERE id = ?1", params![id])
            .context("deleting note")?;
        Ok(changed > 0)
    }

    /// All notes with `done = 0`, oldest first, capped at `limit`.
    pub fn list_open(&self, limit: usize) -> Result<Vec<Note>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, content, created_at, due_at, done FROM notes
             WHERE done = 0 ORDER BY created_at ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_note)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing open notes")
    }

    /// Everything, most recent first — for a `note_list(include_done=true)` view.
    pub fn list_all(&self, limit: usize) -> Result<Vec<Note>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, content, created_at, due_at, done FROM notes
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], row_to_note)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing notes")
    }
}

fn row_to_note(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    let created_raw: String = row.get(2)?;
    let due_raw: Option<String> = row.get(3)?;
    let done_int: i64 = row.get(4)?;
    Ok(Note {
        id: row.get(0)?,
        content: row.get(1)?,
        created_at: DateTime::parse_from_rfc3339(&created_raw)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        due_at: due_raw.and_then(|s| {
            DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|d| d.with_timezone(&Utc))
        }),
        done: done_int != 0,
    })
}

const PROMPT_NOTES_MAX: usize = 30;

/// Render open notes as a system-prompt block, or an empty string if there
/// are none (so callers can always append this without an `if`).
///
/// elfClaw: each line carries its own date so the model can reason about
/// "how long has this been open" instead of the notes floating with no
/// sense of time — a big part of what made recall feel incoherent before.
pub fn open_notes_for_prompt(store: &NoteStore) -> String {
    let notes = match store.list_open(PROMPT_NOTES_MAX) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("Failed to load open notes for prompt: {e}");
            return String::new();
        }
    };
    if notes.is_empty() {
        return String::new();
    }

    let mut out = String::from("\n\n## 未完成的记事\n\n");
    for note in &notes {
        let created = note.created_at.format("%Y-%m-%d");
        match note.due_at {
            Some(due) => {
                let _ = std::fmt::Write::write_fmt(
                    &mut out,
                    format_args!(
                        "- [{created}，到期 {}] {}\n",
                        due.format("%Y-%m-%d %H:%M UTC"),
                        note.content
                    ),
                );
            }
            None => {
                let _ = std::fmt::Write::write_fmt(
                    &mut out,
                    format_args!("- [{created}] {}\n", note.content),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store(tmp: &TempDir) -> NoteStore {
        NoteStore::open(tmp.path()).unwrap()
    }

    #[test]
    fn add_and_list_open() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        store.add("买牛奶", None).unwrap();
        store.add("周五下午三点带孩子看牙医", None).unwrap();

        let open = store.list_open(10).unwrap();
        assert_eq!(open.len(), 2);
        assert_eq!(open[0].content, "买牛奶");
        assert!(!open[0].done);
    }

    #[test]
    fn mark_done_removes_from_open_list() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let note = store.add("买牛奶", None).unwrap();

        assert!(store.mark_done(&note.id).unwrap());
        assert!(store.list_open(10).unwrap().is_empty());

        let all = store.list_all(10).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].done);
    }

    #[test]
    fn mark_done_on_missing_id_returns_false() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        assert!(!store.mark_done("does-not-exist").unwrap());
    }

    #[test]
    fn delete_removes_note_entirely() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let note = store.add("买牛奶", None).unwrap();
        assert!(store.delete(&note.id).unwrap());
        assert!(store.list_all(10).unwrap().is_empty());
    }

    #[test]
    fn due_at_round_trips() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let due = Utc::now() + chrono::Duration::days(3);
        let note = store.add("交电费", Some(due)).unwrap();

        let reloaded = &store.list_open(10).unwrap()[0];
        assert_eq!(reloaded.id, note.id);
        // RFC3339 round-trip can lose sub-second precision; compare to the second.
        assert_eq!(reloaded.due_at.unwrap().timestamp(), due.timestamp());
    }

    #[test]
    fn open_notes_for_prompt_is_empty_string_when_no_notes() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        assert_eq!(open_notes_for_prompt(&store), "");
    }

    #[test]
    fn open_notes_for_prompt_includes_content_and_date() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        store.add("买牛奶", None).unwrap();

        let rendered = open_notes_for_prompt(&store);
        assert!(rendered.contains("买牛奶"));
        assert!(rendered.contains(&Utc::now().format("%Y-%m-%d").to_string()));
    }

    #[test]
    fn open_notes_for_prompt_excludes_done_notes() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        let note = store.add("买牛奶", None).unwrap();
        store.add("交电费", None).unwrap();
        store.mark_done(&note.id).unwrap();

        let rendered = open_notes_for_prompt(&store);
        assert!(!rendered.contains("买牛奶"));
        assert!(rendered.contains("交电费"));
    }

    #[test]
    fn open_notes_for_prompt_caps_at_prompt_notes_max() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp);
        for i in 0..(PROMPT_NOTES_MAX + 10) {
            store.add(&format!("note {i}"), None).unwrap();
        }
        let open = store.list_open(PROMPT_NOTES_MAX).unwrap();
        assert_eq!(open.len(), PROMPT_NOTES_MAX);
    }
}
