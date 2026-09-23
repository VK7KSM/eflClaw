use super::traits::{Tool, ToolResult};
use crate::memory::notes::NoteStore;
use async_trait::async_trait;
use chrono::DateTime;
use serde_json::json;
use std::sync::Arc;

/// elfClaw 2026-09-23: save a note/reminder ("记事"). See elfclaw.md §7 —
/// this is deliberately separate from `memory_store`/embedding-based
/// memory: no vector search, no risk of a real note getting buried under
/// auto-saved chat noise, and (with `due_at` set) it doubles as a reminder
/// that a `JobType::Message` cron job can fire without any LLM call.
pub struct NoteAddTool {
    notes: Arc<NoteStore>,
}

impl NoteAddTool {
    pub fn new(notes: Arc<NoteStore>) -> Self {
        Self { notes }
    }
}

#[async_trait]
impl Tool for NoteAddTool {
    fn name(&self) -> &str {
        "note_add"
    }

    fn description(&self) -> &str {
        "记下一件事（记事/提醒）。content 是要记住的内容；due_at 可选，\
         是 RFC3339 时间戳，设置后这条记事同时也是一个到期提醒。\
         这和 memory_store 不一样——记事永远会在未完成时原样出现在你的系统提示词里，\
         不需要检索，也不会被自动保存的聊天记录挤掉。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "要记住的内容"
                },
                "due_at": {
                    "type": "string",
                    "description": "可选，RFC3339 时间戳（如 2026-09-26T15:00:00+10:00）。设置后这条记事也是一个到期提醒。"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let content = match args.get("content").and_then(serde_json::Value::as_str) {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'content' parameter".to_string()),
                });
            }
        };

        let due_at = match args.get("due_at").and_then(serde_json::Value::as_str) {
            Some(raw) => match DateTime::parse_from_rfc3339(raw) {
                Ok(dt) => Some(dt.with_timezone(&chrono::Utc)),
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid 'due_at' (expected RFC3339): {e}")),
                    });
                }
            },
            None => None,
        };

        match self.notes.add(content, due_at) {
            Ok(note) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&note)?,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to save note: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tool() -> (TempDir, NoteAddTool) {
        let tmp = TempDir::new().unwrap();
        let notes = Arc::new(NoteStore::open(tmp.path()).unwrap());
        (tmp, NoteAddTool::new(notes))
    }

    #[tokio::test]
    async fn adds_note_without_due_date() {
        let (_tmp, tool) = tool();
        let result = tool.execute(json!({"content": "买牛奶"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("买牛奶"));
    }

    #[tokio::test]
    async fn adds_note_with_due_date() {
        let (_tmp, tool) = tool();
        let result = tool
            .execute(json!({"content": "交电费", "due_at": "2026-09-26T15:00:00+10:00"}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("due_at"));
    }

    #[tokio::test]
    async fn rejects_missing_content() {
        let (_tmp, tool) = tool();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn rejects_invalid_due_at() {
        let (_tmp, tool) = tool();
        let result = tool
            .execute(json!({"content": "x", "due_at": "not a timestamp"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("due_at"));
    }
}
