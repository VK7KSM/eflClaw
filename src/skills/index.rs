//! skills.db — lightweight SQLite index for skill discovery.
//!
//! Schema:
//!   skills(name TEXT PRIMARY KEY, description TEXT, category TEXT, risk TEXT)
//!
//! Populated from `workspace/skills/skills_index.json` on first run (table empty).
//! Agent maintains it via sqlite_query tool; see workers/skill_lister.md.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Ensure skills.db exists with schema; seed from skills_index.json if empty.
///
/// Idempotent — safe to call on every daemon startup.
pub fn ensure_skills_db(workspace_dir: &Path) -> Result<()> {
    let skills_dir = workspace_dir.join("skills");
    if !skills_dir.exists() {
        return Ok(()); // skills dir not deployed, skip silently
    }

    let db_path = skills_dir.join("skills.db");
    let conn = Connection::open(&db_path)?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous  = NORMAL;
         CREATE TABLE IF NOT EXISTS skills (
             name        TEXT PRIMARY KEY,
             description TEXT NOT NULL DEFAULT '',
             category    TEXT DEFAULT '',
             risk        TEXT DEFAULT ''
         );
         CREATE INDEX IF NOT EXISTS idx_skills_name ON skills(name);",
    )?;

    // Check if already seeded
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))?;
    if count > 0 {
        return Ok(());
    }

    // Seed from skills_index.json if present
    let index_path = skills_dir.join("skills_index.json");
    if !index_path.exists() {
        tracing::debug!("skills_index.json not found — skills.db left empty");
        return Ok(());
    }

    let json_str = std::fs::read_to_string(&index_path)?;
    let entries: Vec<serde_json::Value> = serde_json::from_str(&json_str)?;

    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO skills (name, description, category, risk) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for e in &entries {
            let name = e
                .get("id")
                .or_else(|| e.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let description = e.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let category = e.get("category").and_then(|v| v.as_str()).unwrap_or("");
            let risk = e.get("risk").and_then(|v| v.as_str()).unwrap_or("");
            stmt.execute(rusqlite::params![name, description, category, risk])?;
        }
    }
    tx.commit()?;

    tracing::info!(
        "skills.db seeded: {} skills from skills_index.json",
        entries.len()
    );
    Ok(())
}
