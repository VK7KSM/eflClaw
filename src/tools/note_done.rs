use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::memory::notes::NoteStore;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct NoteDoneTool {
    notes: Arc<NoteStore>,
    config: Arc<Config>,
}

impl NoteDoneTool {
    pub fn new(notes: Arc<NoteStore>, config: Arc<Config>) -> Self {
        Self { notes, config }
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
            Ok(true) => {
                // elfClaw 2026-09-24: a finished note must not still remind.
                let job_name = super::note_add::note_reminder_job_name(id);
                let cancelled =
                    crate::cron::remove_jobs_by_name(&self.config, &job_name).unwrap_or(0);
                let output = if cancelled > 0 {
                    format!("Marked note {id} as done and cancelled its pending reminder")
                } else {
                    format!("Marked note {id} as done")
                };
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
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
        let (tmp, _config, notes, tool) = setup_with_config();
        (tmp, notes, tool)
    }

    fn setup_with_config() -> (TempDir, Arc<Config>, Arc<NoteStore>, NoteDoneTool) {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.heartbeat.target = Some("telegram".into());
        config.heartbeat.to = Some("zeroclaw_user".into());
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let config = Arc::new(config);
        let notes = Arc::new(NoteStore::open(&config.workspace_dir).unwrap());
        let tool = NoteDoneTool::new(notes.clone(), config.clone());
        (tmp, config, notes, tool)
    }

    #[tokio::test]
    async fn marking_done_cancels_the_pending_reminder() {
        let (_tmp, config, notes, tool) = setup_with_config();
        let add = super::super::note_add::NoteAddTool::new(notes.clone(), config.clone());
        let due = (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339();
        let added = add
            .execute(json!({"content": "交电费", "due_at": due}))
            .await
            .unwrap();
        assert!(added.success, "{:?}", added.error);
        assert_eq!(crate::cron::list_jobs(&config).unwrap().len(), 1);

        let id = notes.list_open(10).unwrap()[0].id.clone();
        let result = tool.execute(json!({"id": id})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("cancelled"));
        assert!(
            crate::cron::list_jobs(&config).unwrap().is_empty(),
            "a finished note must not still remind"
        );
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
