//! What has been pushed, so a restart neither loses nor repeats anything
//! (`workspace/state/cctv.db`). The stream cursor is saved only after an
//! event's pushes went out; `pushed` makes the replay after a crash between
//! the two harmless.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// The connection sits behind a mutex so the store can live in an async
/// task that moves between threads; every call locks only for its own query.
pub struct Store {
    conn: parking_lot::Mutex<Connection>,
}

/// One Telegram message an event produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub chat: String,
    pub message_id: i64,
    pub is_photo: bool,
    pub caption: String,
}

/// What was pushed for an event so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pushed {
    pub level: u8,
    pub has_description: bool,
    pub clip_done: bool,
    pub marked_false: bool,
}

impl Store {
    pub fn open(workspace: &Path) -> Result<Self> {
        let dir = workspace.join("state");
        std::fs::create_dir_all(&dir)?;
        let conn = Connection::open(dir.join("cctv.db"))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS pushed (
                 event_id INTEGER PRIMARY KEY,
                 level INTEGER NOT NULL,
                 has_description INTEGER NOT NULL DEFAULT 0,
                 clip_done INTEGER NOT NULL DEFAULT 0,
                 marked_false INTEGER NOT NULL DEFAULT 0,
                 at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE TABLE IF NOT EXISTS messages (
                 event_id INTEGER NOT NULL,
                 chat TEXT NOT NULL,
                 message_id INTEGER NOT NULL,
                 is_photo INTEGER NOT NULL,
                 caption TEXT NOT NULL,
                 PRIMARY KEY (event_id, chat)
             );
             CREATE TABLE IF NOT EXISTS jobs (
                 job_id TEXT PRIMARY KEY,
                 chat TEXT NOT NULL,
                 at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );",
        )?;
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn pushed(&self, event_id: i64) -> Result<Option<Pushed>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT level, has_description, clip_done, marked_false FROM pushed WHERE event_id = ?1",
                [event_id],
                |r| {
                    Ok(Pushed {
                        level: r.get(0)?,
                        has_description: r.get::<_, i64>(1)? != 0,
                        clip_done: r.get::<_, i64>(2)? != 0,
                        marked_false: r.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn set_pushed(&self, event_id: i64, p: Pushed) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO pushed (event_id, level, has_description, clip_done, marked_false)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event_id,
                p.level,
                i64::from(p.has_description),
                i64::from(p.clip_done),
                i64::from(p.marked_false)
            ],
        )?;
        Ok(())
    }

    pub fn messages(&self, event_id: i64) -> Result<Vec<Sent>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT chat, message_id, is_photo, caption FROM messages WHERE event_id = ?1",
        )?;
        let rows = stmt.query_map([event_id], |r| {
            Ok(Sent {
                chat: r.get(0)?,
                message_id: r.get(1)?,
                is_photo: r.get::<_, i64>(2)? != 0,
                caption: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn save_message(&self, event_id: i64, s: &Sent) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO messages (event_id, chat, message_id, is_photo, caption)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event_id,
                s.chat,
                s.message_id,
                i64::from(s.is_photo),
                s.caption
            ],
        )?;
        Ok(())
    }

    pub fn save_job(&self, job_id: &str, chat: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT OR REPLACE INTO jobs (job_id, chat) VALUES (?1, ?2)",
            params![job_id, chat],
        )?;
        Ok(())
    }

    /// The chat that asked for `job_id`, removing the record.
    pub fn take_job(&self, job_id: &str) -> Result<Option<String>> {
        let chat: Option<String> = self
            .conn
            .lock()
            .query_row("SELECT chat FROM jobs WHERE job_id = ?1", [job_id], |r| {
                r.get(0)
            })
            .optional()?;
        self.conn
            .lock()
            .execute("DELETE FROM jobs WHERE job_id = ?1", [job_id])?;
        Ok(chat)
    }

    /// Forget events older than two weeks (their clips are long gone).
    pub fn prune(&self) -> Result<()> {
        self.conn.lock().execute_batch(
            "DELETE FROM messages WHERE event_id IN
                 (SELECT event_id FROM pushed WHERE at < datetime('now', '-14 days'));
             DELETE FROM pushed WHERE at < datetime('now', '-14 days');
             DELETE FROM jobs WHERE at < datetime('now', '-2 days');",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_pushes_messages_and_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        assert_eq!(s.get("cursor").unwrap(), None);
        s.set("cursor", "10240").unwrap();
        assert_eq!(s.get("cursor").unwrap().as_deref(), Some("10240"));

        assert_eq!(s.pushed(7).unwrap(), None);
        let p = Pushed {
            level: 2,
            ..Pushed::default()
        };
        s.set_pushed(7, p).unwrap();
        s.set_pushed(
            7,
            Pushed {
                has_description: true,
                ..p
            },
        )
        .unwrap();
        assert!(s.pushed(7).unwrap().unwrap().has_description);

        let m = Sent {
            chat: "100".into(),
            message_id: 5,
            is_photo: true,
            caption: "c".into(),
        };
        s.save_message(7, &m).unwrap();
        assert_eq!(s.messages(7).unwrap(), vec![m]);

        s.save_job("c_1", "100").unwrap();
        assert_eq!(s.take_job("c_1").unwrap().as_deref(), Some("100"));
        assert_eq!(s.take_job("c_1").unwrap(), None);
        s.prune().unwrap();
    }
}
