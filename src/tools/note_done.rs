use super::traits::{Tool, ToolResult};
use crate::memory::notes::NoteStore;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct NoteDoneTool {
    notes: Arc<NoteStore>,
}

impl NoteDoneTool {
    pub fn new(notes: Arc<NoteStore>) -> Self {
        Self { notes }
    }
}

#[async_trait]
impl Tool for NoteDoneTool {
    fn name(&self) -> &str {
        "note_done"
    }

    fn description(&self) -> &str {
        "把一条记事标记为已完成——用户说\"这件事办完了/不用再提醒了\"时调用。\
         已完成的记事不会再出现在系统提示词里，但不会被删除（用 note_list(include_done=true) 还能看到）。\
         id 来自 note_add 的返回结果，或系统提示词'未完成的记事'区块（如果你需要 id，先用 note_list 查一下）。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let id = match args.get("id").and_then(serde_json::Value::as_str) {
            Some(id) if !id.trim().is_empty() => id,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'id' parameter".to_string()),
                });
            }
        };

        match self.notes.mark_done(id) {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Marked note {id} as done"),
                error: None,
            }),
            Ok(false) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("No note with id '{id}'")),
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to mark note done: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Arc<NoteStore>, NoteDoneTool) {
        let tmp = TempDir::new().unwrap();
        let notes = Arc::new(NoteStore::open(tmp.path()).unwrap());
        let tool = NoteDoneTool::new(notes.clone());
        (tmp, notes, tool)
    }

    #[tokio::test]
    async fn marks_existing_note_done() {
        let (_tmp, notes, tool) = setup();
        let note = notes.add("买牛奶", None).unwrap();

        let result = tool.execute(json!({"id": note.id})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(notes.list_open(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_on_missing_id() {
        let (_tmp, _notes, tool) = setup();
        let result = tool.execute(json!({"id": "no-such-id"})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn rejects_missing_id_param() {
        let (_tmp, _notes, tool) = setup();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
    }
}
