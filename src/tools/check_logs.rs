// elfClaw: check_logs tool — query elfclaw-logs.db directly (no shell needed)
//
// Lets the agent inspect structured runtime logs (tool calls, cron jobs, LLM calls,
// channel messages, errors) without resorting to shell commands that may be restricted.

use super::traits::{Tool, ToolResult};
use crate::elfclaw_log::LogLevel;
use async_trait::async_trait;
use serde_json::json;

pub struct CheckLogsTool;

impl CheckLogsTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for CheckLogsTool {
    fn name(&self) -> &str {
        "check_logs"
    }

    fn description(&self) -> &str {
        "Log inspection tool — queries state/elfclaw-logs.db directly without shell commands. \
         USER-INITIATED ONLY: Call only when the user explicitly requests log inspection in their \
         current message. Do NOT call automatically when errors occur, tools fail, or cron jobs \
         fail — system errors are not a trigger for this tool. \
         Exception: when called from within an authorized self-check diagnostic session, \
         log access is pre-authorized and no additional confirmation is needed. \
         Prefer over shell commands (tail/cat/grep/Get-Content) which are unreliable on Windows."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max entries to return (default 20, max 100)",
                    "default": 20
                },
                "level": {
                    "type": "string",
                    "enum": ["debug", "info", "warn", "error"],
                    "description": "Filter by log level (omit for all levels)"
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "agent_lifecycle", "llm_call", "tool_call", "cron_job",
                        "heartbeat", "channel_message", "worker_status", "system"
                    ],
                    "description": "Filter by category (omit for all categories)"
                },
                "since_minutes": {
                    "type": "integer",
                    "description": "Only show logs from the last N minutes (e.g. 30)"
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // elfClaw: hard gate — only available when user explicitly opens via /selfcheck
        // elfClaw: return success:true so weak models don't try to compensate
        if !crate::tools::self_check::SelfCheckGate::is_open() {
            return Ok(ToolResult {
                success: true,
                output: "check_logs is currently disabled. \
                         Please tell the user: 日志查看功能当前未启用。如需启用，请发送 /selfcheck 命令。"
                    .into(),
                error: None,
            });
        }

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .min(100) as usize;
        let level = args.get("level").and_then(|v| v.as_str());
        let category = args.get("category").and_then(|v| v.as_str());
        let since_minutes = args.get("since_minutes").and_then(|v| v.as_u64());

        let entries = crate::elfclaw_log::query_recent(limit, level, category, since_minutes);

        if entries.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No log entries found matching the filter.".into(),
                error: None,
            });
        }

        let mut output = format!("## elfClaw Logs ({} entries)\n\n", entries.len());
        for entry in &entries {
            // Slice timestamp to "MM-DDThh:mm" (positions 5..16 of RFC3339)
            let ts = if entry.timestamp.len() >= 16 {
                &entry.timestamp[5..16]
            } else {
                entry.timestamp.as_str()
            };

            output.push_str(&format!(
                "[{}] {:5} {:15} {}: {}\n",
                ts,
                entry.level.as_str().to_uppercase(),
                entry.category.as_str(),
                entry.component,
                entry.message
            ));

            // Include details JSON for warn/error entries
            if matches!(entry.level, LogLevel::Warn | LogLevel::Error)
                && entry.details != serde_json::Value::Null
            {
                output.push_str(&format!("    details: {}\n", entry.details));
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}
