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
        "PREFERRED tool for log inspection and system diagnostics. \
         When asked to view, check, query, inspect, or read system logs, runtime records, \
         errors, or execution history — use this tool INSTEAD OF shell commands. \
         Shell commands (tail, cat, grep, Get-Content, ls) are unreliable on Windows \
         and may be blocked by security policy. This tool directly queries \
         state/elfclaw-logs.db with zero shell commands. \
         Use for: checking recent errors, tracing tool call failures, \
         viewing cron job history, verifying if actions completed, diagnosing system issues."
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
