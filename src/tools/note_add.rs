use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::memory::notes::NoteStore;
use async_trait::async_trait;
use chrono::DateTime;
use serde_json::json;
use std::sync::Arc;

/// elfClaw 2026-09-23: save a note/reminder ("记事"). See elfclaw.md §7 —
/// this is deliberately separate from `memory_store`/embedding-based
/// memory: no vector search, no risk of a real note getting buried under
/// auto-saved chat noise.
///
/// elfClaw 2026-09-24: with `due_at` set, this now actually creates the
/// reminder — a one-shot `JobType::Message` cron job named `note:<id>`
/// that code delivers at `due_at` with no LLM call. Before this, `due_at`
/// was only stored: the tool told the model "this is a reminder" but
/// nothing ever fired. If the reminder can't be created the note is rolled
/// back and an error returned, so the model never believes a reminder
/// exists when it doesn't.
pub struct NoteAddTool {
    notes: Arc<NoteStore>,
    config: Arc<Config>,
}

impl NoteAddTool {
    pub fn new(notes: Arc<NoteStore>, config: Arc<Config>) -> Self {
        Self { notes, config }
    }
}

/// Name of the reminder job belonging to a note.
pub(crate) fn note_reminder_job_name(note_id: &str) -> String {
    format!("note:{note_id}")
}

/// Where a reminder is delivered: the chat it was set from, else the
/// configured heartbeat target.
fn reminder_target(config: &Config) -> Option<(String, String)> {
    if let Some(caller) = super::caller_context::current_caller() {
        if !caller.channel.is_empty() && !caller.sender.is_empty() && caller.channel != "cli" {
            return Some((caller.channel, caller.sender));
        }
    }
    match (&config.heartbeat.target, &config.heartbeat.to) {
        (Some(channel), Some(to)) if !channel.is_empty() && !to.is_empty() => {
            Some((channel.clone(), to.clone()))
        }
        _ => None,
    }
}

#[async_trait]
impl Tool for NoteAddTool {
    fn name(&self) -> &str {
        "note_add"
    }

    fn description(&self) -> &str {
        "记下一件事（记事/提醒）。content 是要记住的内容；due_at 可选，\
         是 RFC3339 时间戳，设置后到点会由系统自动把提醒发给用户（不需要再调用 cron_add）。\
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

        let target = match due_at {
            Some(_) => match reminder_target(&self.config) {
                Some(t) => Some(t),
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Cannot tell where to deliver the reminder (no chat context and no \
[heartbeat] target/to configured); note not saved."
                                .to_string(),
                        ),
                    });
                }
            },
            None => None,
        };

        let note = match self.notes.add(content, due_at) {
            Ok(note) => note,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to save note: {e}")),
                });
            }
        };

        let mut out = serde_json::to_value(&note)?;
        if let (Some(due), Some((channel, to))) = (due_at, target) {
            let job = crate::cron::add_message_job(
                &self.config,
                Some(note_reminder_job_name(&note.id)),
                crate::cron::Schedule::At { at: due },
                &format!("⏰ 提醒：{content}"),
                Some(crate::cron::DeliveryConfig {
                    mode: "announce".into(),
                    channel: Some(channel),
                    to: Some(to),
                    best_effort: false,
                }),
                true,
            );
            match job {
                Ok(job) => {
                    out["reminder"] = json!({ "job": job.name, "fires_at": job.next_run });
                }
                Err(e) => {
                    let _ = self.notes.delete(&note.id);
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Reminder could not be scheduled ({e}); note not saved."
                        )),
                    });
                }
            }
        }

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&out)?,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir, heartbeat_target: bool) -> Arc<Config> {
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        if heartbeat_target {
            config.heartbeat.target = Some("telegram".into());
            config.heartbeat.to = Some("zeroclaw_user".into());
        }
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        Arc::new(config)
    }

    fn tool_with(heartbeat_target: bool) -> (TempDir, Arc<Config>, Arc<NoteStore>, NoteAddTool) {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp, heartbeat_target);
        let notes = Arc::new(NoteStore::open(&config.workspace_dir).unwrap());
        let tool = NoteAddTool::new(notes.clone(), config.clone());
        (tmp, config, notes, tool)
    }

    fn future_rfc3339() -> String {
        (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339()
    }

    #[tokio::test]
    async fn adds_note_without_due_date_and_no_reminder_job() {
        let (_tmp, config, _notes, tool) = tool_with(true);
        let result = tool.execute(json!({"content": "买牛奶"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("买牛奶"));
        assert!(crate::cron::list_jobs(&config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn due_date_actually_schedules_a_reminder_job() {
        // Regression: due_at used to be stored only — nothing ever fired.
        let (_tmp, config, notes, tool) = tool_with(true);
        let result = tool
            .execute(json!({"content": "交电费", "due_at": future_rfc3339()}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let note = &notes.list_open(10).unwrap()[0];
        let jobs = crate::cron::list_jobs(&config).unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(
            job.name.as_deref(),
            Some(note_reminder_job_name(&note.id).as_str())
        );
        assert_eq!(job.job_type, crate::cron::JobType::Message);
        assert!(job.prompt.as_deref().unwrap().contains("交电费"));
        assert_eq!(job.delivery.channel.as_deref(), Some("telegram"));
        assert_eq!(job.delivery.to.as_deref(), Some("zeroclaw_user"));
        assert!(job.delete_after_run);
        assert_eq!(job.next_run.timestamp(), note.due_at.unwrap().timestamp());
    }

    #[tokio::test]
    async fn reminder_goes_back_to_the_chat_it_was_set_from() {
        use super::super::caller_context::{CallerInfo, CALLER_INFO};
        let (_tmp, config, _notes, tool) = tool_with(true);
        let caller = CallerInfo {
            channel: "telegram".into(),
            sender: "zeroclaw_chat_b".into(),
        };
        let result = CALLER_INFO
            .scope(
                caller,
                tool.execute(json!({"content": "x", "due_at": future_rfc3339()})),
            )
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
        let job = &crate::cron::list_jobs(&config).unwrap()[0];
        assert_eq!(job.delivery.to.as_deref(), Some("zeroclaw_chat_b"));
    }

    #[tokio::test]
    async fn past_due_date_is_rejected_and_note_rolled_back() {
        let (_tmp, config, notes, tool) = tool_with(true);
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let result = tool
            .execute(json!({"content": "过去的事", "due_at": past}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            notes.list_all(10).unwrap().is_empty(),
            "note must be rolled back"
        );
        assert!(crate::cron::list_jobs(&config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_delivery_target_means_no_note_and_an_error() {
        let (_tmp, _config, notes, tool) = tool_with(false);
        let result = tool
            .execute(json!({"content": "x", "due_at": future_rfc3339()}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(notes.list_all(10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_missing_content() {
        let (_tmp, _config, _notes, tool) = tool_with(true);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn rejects_invalid_due_at() {
        let (_tmp, _config, _notes, tool) = tool_with(true);
        let result = tool
            .execute(json!({"content": "x", "due_at": "not a timestamp"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("due_at"));
    }
}
