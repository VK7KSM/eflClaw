// elfClaw: sqlite_query tool — agent-accessible SQLite query interface.
//
// Designed primarily for querying and maintaining workspace/skills/skills.db,
// but works on any workspace-owned .db file.
//
// Security model:
//   - SELECT / INSERT / UPDATE / DELETE are allowed on non-system databases.
//   - DDL (DROP / CREATE / ALTER / TRUNCATE) is blocked — prevents schema destruction.
//   - ATTACH / DETACH is blocked — prevents workspace-boundary bypass.
//   - PRAGMA is blocked — some pragmas are writable and can corrupt journals.
//   - System databases (elfclaw-logs.db, brain.db, jobs.db, cron.db) are blocked.
//   - Path must pass SecurityPolicy workspace-confinement checks.

use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// System-database filename suffixes that must never be written to by the agent.
const SYSTEM_DBS: &[&str] = &[
    "elfclaw-logs.db",
    "brain.db",
    "jobs.db",
    "cron.db",
];

/// SQL statement verbs that are allowed.
const ALLOWED_VERBS: &[&str] = &["select", "insert", "update", "delete", "with"];

/// SQL statement verbs that are explicitly blocked (DDL + dangerous pragmas).
const BLOCKED_VERBS: &[&str] = &[
    "drop", "create", "alter", "truncate", "attach", "detach", "pragma",
];

pub struct SqliteQueryTool {
    security: Arc<SecurityPolicy>,
}

impl SqliteQueryTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for SqliteQueryTool {
    fn name(&self) -> &str {
        "sqlite_query"
    }

    fn description(&self) -> &str {
        "Execute a SQL query against a SQLite database file within the workspace.\n\
         PRIMARY USE: query or maintain workspace/skills/skills.db for skill discovery.\n\
         Allowed: SELECT, INSERT, UPDATE, DELETE on non-system databases.\n\
         BLOCKED: DDL (DROP/CREATE/ALTER/TRUNCATE), ATTACH/PRAGMA, system databases\n\
         \x20 (elfclaw-logs.db, brain.db, jobs.db, cron.db).\n\
         Use ONLY for workspace data files you explicitly own.\n\
         db_path is relative to the workspace directory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "db_path": {
                    "type": "string",
                    "description": "Path to the SQLite database file, relative to the workspace directory. Example: \"skills/skills.db\""
                },
                "sql": {
                    "type": "string",
                    "description": "SQL statement to execute. SELECT returns a table. INSERT/UPDATE/DELETE returns rows affected."
                }
            },
            "required": ["db_path", "sql"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let db_path_raw = args["db_path"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let sql = args["sql"].as_str().unwrap_or("").trim().to_string();

        if db_path_raw.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("db_path is required".to_string()),
            });
        }
        if sql.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("sql is required".to_string()),
            });
        }

        // --- Security check 1: SQL verb allowlist / blocklist ---
        let first_word = sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();

        if BLOCKED_VERBS.contains(&first_word.as_str()) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "SQL verb '{}' is not permitted. Allowed: SELECT, INSERT, UPDATE, DELETE.",
                    first_word
                )),
            });
        }
        if !ALLOWED_VERBS.contains(&first_word.as_str()) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "SQL verb '{}' is not recognised. Allowed: SELECT, INSERT, UPDATE, DELETE.",
                    first_word
                )),
            });
        }

        // --- Security check 2: system database protection ---
        let db_lower = db_path_raw.to_lowercase().replace('\\', "/");
        for sys_db in SYSTEM_DBS {
            if db_lower.ends_with(sys_db) {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Access to system database '{}' is not permitted.",
                        sys_db
                    )),
                });
            }
        }

        // --- Security check 3: path allowlist via SecurityPolicy ---
        if !self.security.is_path_allowed(&db_path_raw) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Path '{}' is not permitted by security policy.",
                    db_path_raw
                )),
            });
        }

        // Resolve the absolute path (join workspace_dir + relative path).
        let workspace_dir = self.security.workspace_dir.clone();
        let abs_path = workspace_dir.join(&db_path_raw);

        // Canonicalise if possible (file must exist for writes too, so we
        // normalise without requiring canonical to succeed on missing files).
        let resolved = match tokio::fs::canonicalize(&abs_path).await {
            Ok(p) => p,
            Err(_) => abs_path.clone(),
        };

        if !self.security.is_resolved_path_allowed(&resolved) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Resolved path '{}' is outside the allowed workspace.",
                    resolved.display()
                )),
            });
        }

        let is_readonly = first_word == "select" || first_word == "with";
        let abs_path_clone = abs_path.clone();
        let sql_clone = sql.clone();

        let result = tokio::task::spawn_blocking(move || {
            run_query(&abs_path_clone, &sql_clone, is_readonly)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))??;

        Ok(result)
    }
}

/// Execute a SQL query synchronously inside a blocking thread.
fn run_query(
    db_path: &std::path::Path,
    sql: &str,
    is_readonly: bool,
) -> anyhow::Result<ToolResult> {
    use rusqlite::{Connection, OpenFlags};

    let flags = if is_readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    };

    let conn = Connection::open_with_flags(db_path, flags)?;

    if is_readonly {
        // SELECT / WITH — return a formatted text table
        let mut stmt = conn.prepare(sql)?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();

        let mut rows_out: Vec<Vec<String>> = Vec::new();
        let mut query_rows = stmt.query([])?;
        while let Some(row) = query_rows.next()? {
            let vals: Vec<String> = (0..col_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(|v| match v {
                            rusqlite::types::ValueRef::Null => "NULL".to_string(),
                            rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                            rusqlite::types::ValueRef::Real(f) => f.to_string(),
                            rusqlite::types::ValueRef::Text(t) => {
                                String::from_utf8_lossy(t).to_string()
                            }
                            rusqlite::types::ValueRef::Blob(b) => {
                                format!("<blob {} bytes>", b.len())
                            }
                        })
                        .unwrap_or_else(|_| "ERR".to_string())
                })
                .collect();
            rows_out.push(vals);
        }

        if rows_out.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "(no rows)".to_string(),
                error: None,
            });
        }

        // Calculate column widths for alignment
        let mut widths: Vec<usize> = col_names.iter().map(|s| s.len()).collect();
        for row in &rows_out {
            for (i, val) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(val.len());
                }
            }
        }

        let sep = widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("-+-");

        let header = col_names
            .iter()
            .enumerate()
            .map(|(i, n)| format!("{:width$}", n, width = widths[i]))
            .collect::<Vec<_>>()
            .join(" | ");

        let mut lines = vec![header, sep];
        for row in &rows_out {
            let line = row
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let w = widths.get(i).copied().unwrap_or(0);
                    format!("{:width$}", v, width = w)
                })
                .collect::<Vec<_>>()
                .join(" | ");
            lines.push(line);
        }
        lines.push(format!("({} rows)", rows_out.len()));

        Ok(ToolResult {
            success: true,
            output: lines.join("\n"),
            error: None,
        })
    } else {
        // INSERT / UPDATE / DELETE — return rows affected
        let affected = conn.execute(sql, [])?;
        Ok(ToolResult {
            success: true,
            output: format!("OK: {} rows affected", affected),
            error: None,
        })
    }
}
