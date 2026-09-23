use super::traits::{Tool, ToolResult};
use crate::memory::notes::NoteStore;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct NoteListTool {
    notes: Arc<NoteStore>,
}

impl NoteListTool {
    pub fn new(notes: Arc<NoteStore>) -> Self {
        Self { notes }
    }
}

#[async_trait]
impl Tool for NoteListTool {
    fn name(&self) -> &str {
        "note_list"
    }

    fn description(&self) -> &str {
        "列出记事。默认只显示未完成的；include_done=true 时也显示已完成的（最近的在前）。\
         注意：未完成的记事已经在你的系统提示词里了，只有在用户明确要求\"查看全部记事\"或\
         \"看看已完成的\"时才需要调用这个工具。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "include_done": {
                    "type": "boolean",
                    "description": "true 显示全部（含已完成），false（默认）只显示未完成的"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let include_done = args
            .get("include_done")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let result = if include_done {
            self.notes.list_all(100)
        } else {
            self.notes.list_open(100)
        };

        match result {
            Ok(notes) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&notes)?,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to list notes: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Arc<NoteStore>, NoteListTool) {
        let tmp = TempDir::new().unwrap();
        let notes = Arc::new(NoteStore::open(tmp.path()).unwrap());
        let tool = NoteListTool::new(notes.clone());
        (tmp, notes, tool)
    }

    #[tokio::test]
    async fn lists_open_notes_by_default() {
        let (_tmp, notes, tool) = setup();
        let done = notes.add("已完成的", None).unwrap();
        notes.mark_done(&done.id).unwrap();
        notes.add("未完成的", None).unwrap();

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("未完成的"));
        assert!(!result.output.contains("已完成的"));
    }

    #[tokio::test]
    async fn include_done_shows_everything() {
        let (_tmp, notes, tool) = setup();
        let done = notes.add("已完成的", None).unwrap();
        notes.mark_done(&done.id).unwrap();

        let result = tool.execute(json!({"include_done": true})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("已完成的"));
    }

    #[tokio::test]
    async fn empty_list_still_succeeds() {
        let (_tmp, _notes, tool) = setup();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output.trim(), "[]");
    }
}
