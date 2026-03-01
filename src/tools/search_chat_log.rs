//! `search_chat_log` tool — Agent searches local chat logs.
//!
//! Owner-gated: only the configured owner can query other users' logs.
//! Non-owners can only search their own logs.

use async_trait::async_trait;
use std::path::PathBuf;

use crate::channels::{chat_index::ChatIndex, chat_log};
use crate::tools::traits::{Tool, ToolResult};

/// Tool that lets the Agent search local chat logs.
pub struct SearchChatLogTool {
    workspace_dir: PathBuf,
    owner: Option<String>,
}

impl SearchChatLogTool {
    pub fn new(workspace_dir: PathBuf, owner: Option<String>) -> Self {
        Self {
            workspace_dir,
            owner,
        }
    }
}

#[async_trait]
impl Tool for SearchChatLogTool {
    fn name(&self) -> &str {
        "search_chat_log"
    }

    fn description(&self) -> &str {
        "Search local chat log files and summaries. Use to recall conversations \
         with a specific user, find past topics, or retrieve recent messages. \
         Only the owner can view other users' chat logs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "chat_name": {
                    "type": "string",
                    "description": "Telegram username to search (e.g. 'Alice'). Required."
                },
                "query": {
                    "type": "string",
                    "description": "Optional keyword to search in summaries (FTS5)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max messages to return. Default: 10."
                },
                "sender": {
                    "type": "string",
                    "description": "Current sender username (auto-filled by system)."
                }
            },
            "required": ["chat_name", "sender"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let chat_name = args
            .get("chat_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let sender = args
            .get("sender")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let query = args.get("query").and_then(|v| v.as_str()).map(String::from);
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

        if chat_name.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("chat_name is required".to_string()),
            });
        }

        // ── Owner-gated access control ──
        let is_owner = self
            .owner
            .as_ref()
            .is_some_and(|o| o.eq_ignore_ascii_case(&sender));

        // Non-owner trying to access someone else's logs
        if !is_owner && !chat_name.eq_ignore_ascii_case(&sender) {
            return Ok(ToolResult {
                success: false,
                output: "这是私密信息，我无法告诉你。".to_string(),
                error: Some(
                    "Permission denied: cross-user chat log access restricted to owner".to_string(),
                ),
            });
        }

        let mut output_parts: Vec<String> = Vec::new();

        // ── 1. Fetch recent raw messages from JSON files ──
        match chat_log::load_recent_messages(&self.workspace_dir, &chat_name, limit) {
            Ok(messages) if !messages.is_empty() => {
                output_parts.push(format!(
                    "## 最近 {} 条消息 ({})\n",
                    messages.len(),
                    chat_name
                ));
                for msg in &messages {
                    let type_tag = if msg.msg_type != "text" {
                        format!(" [{}]", msg.msg_type)
                    } else {
                        String::new()
                    };
                    output_parts.push(format!("- **{}**{}: {}\n", msg.role, type_tag, msg.content));
                }
            }
            Ok(_) => {
                output_parts.push(format!("没有找到与 {} 的聊天记录。\n", chat_name));
            }
            Err(e) => {
                output_parts.push(format!("读取聊天日志出错: {}\n", e));
            }
        }

        // ── 2. Search SQLite index (FTS or user summaries) ──
        match ChatIndex::open(&self.workspace_dir) {
            Ok(index) => {
                let summaries = if let Some(ref q) = query {
                    index.search_fts(q, limit).unwrap_or_default()
                } else {
                    index
                        .get_user_summaries(&chat_name, limit)
                        .unwrap_or_default()
                };

                if !summaries.is_empty() {
                    output_parts.push(format!("\n## 聊天摘要索引\n"));
                    for s in &summaries {
                        let topics = s.topics.as_deref().unwrap_or("-");
                        output_parts.push(format!(
                            "- **{}** ({}): {} [话题: {}, 消息数: {}]\n",
                            s.chat_name, s.date, s.summary, topics, s.msg_count
                        ));
                    }
                }
            }
            Err(e) => {
                tracing::debug!("chat_index not available: {e}");
            }
        }

        let full_output = output_parts.join("");

        Ok(ToolResult {
            success: true,
            output: full_output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn owner_can_access_other_logs() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        // Create test data
        chat_log::append_turn(ws, "1", "Alice", chat_log::text_turn("user", "hello")).unwrap();

        let tool = SearchChatLogTool::new(ws.to_path_buf(), Some("Owner".to_string()));
        let result = tool
            .execute(serde_json::json!({
                "chat_name": "Alice",
                "sender": "Owner"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn non_owner_cannot_access_other_logs() {
        let tmp = TempDir::new().unwrap();
        let tool = SearchChatLogTool::new(tmp.path().to_path_buf(), Some("Owner".to_string()));
        let result = tool
            .execute(serde_json::json!({
                "chat_name": "Owner",
                "sender": "Stranger"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("私密信息"));
    }

    #[tokio::test]
    async fn user_can_access_own_logs() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();

        chat_log::append_turn(ws, "1", "Alice", chat_log::text_turn("user", "my message")).unwrap();

        let tool = SearchChatLogTool::new(ws.to_path_buf(), Some("Owner".to_string()));
        let result = tool
            .execute(serde_json::json!({
                "chat_name": "Alice",
                "sender": "Alice"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("my message"));
    }

    #[tokio::test]
    async fn empty_chat_name_returns_error() {
        let tmp = TempDir::new().unwrap();
        let tool = SearchChatLogTool::new(tmp.path().to_path_buf(), None);
        let result = tool
            .execute(serde_json::json!({
                "chat_name": "",
                "sender": "User"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
