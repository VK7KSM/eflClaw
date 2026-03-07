// elfClaw: self_check tool — pure data collector for source-level diagnostics
//
// Architecture (v0.3.1): zero internal LLM calls.
// - action="collect": sync source → collect logs → search error keywords →
//   read key files → return structured JSON to the main model.
// - action="save_report": receive report text from main model → write to homework/.
//
// The main model (default_model) is responsible for all analysis and report writing.
// This ensures the strongest model participates in the entire diagnostic process.

use super::content_search::ContentSearchTool;
use super::file_read::FileReadTool;
use super::source_sync::SourceSyncTool;
use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::elfclaw_log::{self, LogEntry};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// ── Limits ──────────────────────────────────────────────────────────────────

const MAX_SEARCH_RESULT_CHARS: usize = 3_000;
const MAX_FILE_READ_CHARS: usize = 4_000;
const SYNC_TIMEOUT_SECS: u64 = 120;

/// Keywords to search in source code based on common error patterns.
const ERROR_SEARCH_KEYWORDS: &[(&str, &str)] = &[
    ("provider", "src/providers/"),
    ("tool_call", "src/tools/"),
    ("channel", "src/channels/"),
    ("cron", "src/cron/"),
    ("heartbeat", "src/daemon/"),
    ("agent", "src/agent/"),
    ("gateway", "src/gateway/"),
    ("security", "src/security/"),
];

/// Key files to always include in collect output for context.
const KEY_FILES: &[&str] = &[
    "Cargo.toml",
    "src/tools/mod.rs",
    "src/daemon/mod.rs",
    "src/channels/mod.rs",
];

// ── SelfCheckTool ───────────────────────────────────────────────────────────

pub struct SelfCheckTool {
    security: Arc<SecurityPolicy>,
    config: Arc<Config>,
    source_sync: Arc<SourceSyncTool>,
    content_search: Arc<ContentSearchTool>,
    file_read: Arc<FileReadTool>,
}

impl SelfCheckTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        config: Arc<Config>,
        source_sync: Arc<SourceSyncTool>,
        content_search: Arc<ContentSearchTool>,
        file_read: Arc<FileReadTool>,
    ) -> Self {
        Self {
            security,
            config,
            source_sync,
            content_search,
            file_read,
        }
    }

    /// Resolve the report output directory: config_dir/homework/.
    fn report_dir(&self) -> PathBuf {
        let config_dir = self
            .config
            .config_path
            .parent()
            .unwrap_or(std::path::Path::new("."));
        config_dir.join("homework")
    }

    /// Check whether a source repo directory exists (from a previous sync).
    fn source_dir_exists(&self, repo_id: &str) -> bool {
        let marker = self
            .security
            .workspace_dir
            .join("workspace/github")
            .join(repo_id)
            .join("Cargo.toml");
        marker.exists()
    }

    /// Collect diagnostic data: sync sources, gather logs, search code, read files.
    /// Returns structured JSON — zero LLM calls.
    async fn collect(&self, since_minutes: u64) -> anyhow::Result<ToolResult> {
        // ── Step 1: Sync source repos ─────────────────────────────────
        let mut sync_status: Vec<String> = Vec::new();

        for repo_id in &["elfclaw", "zeroclaw"] {
            match tokio::time::timeout(
                Duration::from_secs(SYNC_TIMEOUT_SECS),
                self.source_sync
                    .execute(json!({"repo_id": repo_id, "action": "sync"})),
            )
            .await
            {
                Ok(Ok(r)) if r.success => {
                    sync_status.push(format!("{repo_id}: {}", first_line(&r.output)));
                }
                Ok(Ok(r)) => {
                    sync_status.push(format!(
                        "{repo_id}: sync failed — {}",
                        r.error.unwrap_or_default()
                    ));
                }
                Ok(Err(e)) => {
                    sync_status.push(format!("{repo_id}: error — {e}"));
                }
                Err(_) => {
                    sync_status.push(format!("{repo_id}: timed out"));
                }
            }
        }

        // ── Anti-hallucination gate: check source dirs exist ──────────
        let has_source_code =
            self.source_dir_exists("elfclaw") || self.source_dir_exists("zeroclaw");

        // ── Step 2: Collect structured logs ─────────────────────────────
        let mut entries: Vec<LogEntry> =
            elfclaw_log::query_recent(100, Some("error"), None, Some(since_minutes));

        if entries.is_empty() {
            entries = elfclaw_log::query_recent(50, Some("warn"), None, Some(since_minutes));
        }

        // Convert log entries to JSON
        let logs_json: Vec<Value> = entries
            .iter()
            .map(|e| {
                json!({
                    "level": e.level.as_str(),
                    "category": e.category.as_str(),
                    "component": &e.component,
                    "message": &e.message,
                    "timestamp": &e.timestamp,
                    "details": &e.details,
                })
            })
            .collect();

        let mode = if has_source_code { "full" } else { "log_only" };

        // If no logs and no source, return early
        if entries.is_empty() && !has_source_code {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "mode": mode,
                    "sync_status": sync_status.join("; "),
                    "logs": [],
                    "search_results": [],
                    "key_files": {},
                    "source_tree": [],
                    "summary": format!("最近 {} 分钟内无 error/warn 日志，源码不可用", since_minutes),
                })
                .to_string(),
                error: None,
            });
        }

        if entries.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "mode": mode,
                    "sync_status": sync_status.join("; "),
                    "logs": [],
                    "search_results": [],
                    "key_files": {},
                    "source_tree": [],
                    "summary": format!("最近 {} 分钟内无 error/warn 日志，系统正常", since_minutes),
                })
                .to_string(),
                error: None,
            });
        }

        let mut search_results: Vec<Value> = Vec::new();
        let mut key_files_map = serde_json::Map::new();
        let mut source_tree: Vec<String> = Vec::new();

        if has_source_code {
            // ── Step 3: Search error keywords in source code ──────────
            // Extract unique keywords from error messages
            let keywords = extract_search_keywords(&entries);

            for keyword in keywords.iter().take(6) {
                // Determine which source directories to search based on error category
                let search_paths = determine_search_paths(&entries);

                for search_path in search_paths.iter().take(4) {
                    let full_path = format!(
                        "{}/workspace/github/elfclaw/{}",
                        self.security.workspace_dir.display(),
                        search_path
                    );

                    if let Ok(r) = self
                        .content_search
                        .execute(json!({
                            "pattern": keyword,
                            "path": &full_path,
                            "output_mode": "content",
                            "context_before": 3,
                            "context_after": 5,
                            "max_results": 10
                        }))
                        .await
                    {
                        if r.success && !r.output.trim().is_empty() {
                            search_results.push(json!({
                                "keyword": keyword,
                                "path": search_path,
                                "matches": truncate_if_needed(&r.output, MAX_SEARCH_RESULT_CHARS),
                            }));
                        }
                    }
                }
            }

            // ── Step 4: Read key files ────────────────────────────────
            for key_file in KEY_FILES {
                let full_path = format!(
                    "{}/workspace/github/elfclaw/{}",
                    self.security.workspace_dir.display(),
                    key_file
                );

                if let Ok(r) = self
                    .file_read
                    .execute(json!({
                        "path": &full_path,
                        "offset": 1,
                        "limit": 80
                    }))
                    .await
                {
                    if r.success {
                        key_files_map.insert(
                            key_file.to_string(),
                            Value::String(truncate_if_needed(&r.output, MAX_FILE_READ_CHARS)),
                        );
                    }
                }
            }

            // ── Step 5: List source tree ──────────────────────────────
            let src_dir = self
                .security
                .workspace_dir
                .join("workspace/github/elfclaw/src");
            if src_dir.exists() {
                if let Ok(entries) = std::fs::read_dir(&src_dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
                            source_tree.push(format!("src/{name}/"));
                        } else {
                            source_tree.push(format!("src/{name}"));
                        }
                    }
                }
                source_tree.sort();
            }
        }

        let result = json!({
            "mode": mode,
            "sync_status": sync_status.join("; "),
            "logs": logs_json,
            "search_results": search_results,
            "key_files": key_files_map,
            "source_tree": source_tree,
        });

        Ok(ToolResult {
            success: true,
            output: result.to_string(),
            error: None,
        })
    }

    /// Save a report written by the main model to homework/.
    async fn save_report(&self, report: &str) -> anyhow::Result<ToolResult> {
        let date = chrono::Local::now().format("%Y-%m-%d");
        let homework_dir = self.report_dir();
        tokio::fs::create_dir_all(&homework_dir).await?;
        let report_file = homework_dir.join(format!("debug_代码修改计划_{date}.md"));
        tokio::fs::write(&report_file, report)
            .await
            .map_err(|e| anyhow::anyhow!("报告写入失败: {e}"))?;
        let report_path = report_file.display().to_string();

        Ok(ToolResult {
            success: true,
            output: format!("报告已保存到: {report_path}"),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for SelfCheckTool {
    fn name(&self) -> &str {
        "self_check"
    }

    fn description(&self) -> &str {
        "Two-phase self-check tool. \
         Phase 1: self_check(action=\"collect\") — syncs source code, collects error logs, \
         searches source for error keywords, reads key files. Returns structured JSON. Zero LLM calls. \
         Phase 2: self_check(action=\"save_report\", report=\"...\") — saves the main model's \
         diagnostic report to homework/. \
         Trigger: 自检/self-check/健康检查/debug自检."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["collect", "save_report"],
                    "description": "Action: 'collect' = gather logs + source data (default), 'save_report' = save report text to file",
                    "default": "collect"
                },
                "since_minutes": {
                    "type": "integer",
                    "description": "Log lookback window in minutes. Default: 60. Only used with action='collect'.",
                    "default": 60
                },
                "report": {
                    "type": "string",
                    "description": "Report text to save. Required when action='save_report'."
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        // ── Security gate ───────────────────────────────────────────────
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Blocked: autonomy is read-only".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Blocked: action rate limit exceeded".into()),
            });
        }

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("collect");

        match action {
            "collect" => {
                let since_minutes = args
                    .get("since_minutes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60);
                self.collect(since_minutes).await
            }
            "save_report" => {
                let report = args
                    .get("report")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| {
                        anyhow::anyhow!("Missing required parameter 'report' for save_report action")
                    })?;
                self.save_report(report).await
            }
            other => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unknown action '{other}'. Use 'collect' or 'save_report'."
                )),
            }),
        }
    }
}

// ── Helper functions ────────────────────────────────────────────────────────

/// Extract the first line of a multi-line string.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Extract search keywords from error log entries.
/// Pulls function names, error codes, and key identifiers from messages.
fn extract_search_keywords(entries: &[LogEntry]) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for entry in entries {
        // Extract component name as a keyword
        if !entry.component.is_empty() && seen.insert(entry.component.clone()) {
            keywords.push(entry.component.clone());
        }

        // Extract key terms from the message (function names, error codes)
        for word in entry.message.split_whitespace() {
            let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            // Look for function-like names (snake_case or camelCase with length > 4)
            if clean.len() > 4
                && clean.chars().all(|c| c.is_alphanumeric() || c == '_')
                && clean.contains('_')
                && seen.insert(clean.to_string())
            {
                keywords.push(clean.to_string());
            }
        }
    }

    // Limit to avoid excessive searches
    keywords.truncate(10);
    keywords
}

/// Determine which source directories to search based on error categories.
fn determine_search_paths(entries: &[LogEntry]) -> Vec<String> {
    let mut paths = std::collections::HashSet::new();

    for entry in entries {
        let cat = entry.category.as_str();
        for (pattern, src_path) in ERROR_SEARCH_KEYWORDS {
            if cat.contains(pattern) {
                paths.insert(src_path.to_string());
            }
        }
    }

    // Always include common directories
    paths.insert("src/agent/".to_string());

    paths.into_iter().collect()
}

/// Truncate string to max_chars with a marker.
fn truncate_if_needed(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        s.to_string()
    } else {
        let mut end = max_chars;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}\n\n[... 截断，共 {} 字符 ...]", &s[..end], s.len())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elfclaw_log::{LogCategory, LogLevel};
    use crate::security::AutonomyLevel;

    fn test_policy(level: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn make_tool(level: AutonomyLevel) -> SelfCheckTool {
        let sec = test_policy(level);
        SelfCheckTool::new(
            sec.clone(),
            Arc::new(Config::default()),
            Arc::new(SourceSyncTool::new(sec.clone())),
            Arc::new(ContentSearchTool::new(sec.clone())),
            Arc::new(FileReadTool::new(sec)),
        )
    }

    fn make_entry(
        level: LogLevel,
        category: LogCategory,
        component: &str,
        message: &str,
        details: Value,
    ) -> LogEntry {
        LogEntry {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: "2026-03-06T12:00:00+08:00".into(),
            level,
            category,
            component: component.into(),
            message: message.into(),
            details,
        }
    }

    #[test]
    fn tool_name() {
        let tool = make_tool(AutonomyLevel::Full);
        assert_eq!(tool.name(), "self_check");
    }

    #[test]
    fn schema_has_action_param() {
        let tool = make_tool(AutonomyLevel::Full);
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["action"].is_object());
        assert!(schema["properties"]["report"].is_object());
    }

    #[tokio::test]
    async fn blocks_readonly() {
        let tool = make_tool(AutonomyLevel::ReadOnly);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn save_report_requires_report_param() {
        let tool = make_tool(AutonomyLevel::Full);
        let result = tool
            .execute(json!({"action": "save_report"}))
            .await;
        assert!(result.is_err() || !result.unwrap().success);
    }

    #[tokio::test]
    async fn rejects_unknown_action() {
        let tool = make_tool(AutonomyLevel::Full);
        let result = tool
            .execute(json!({"action": "destroy"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Unknown action"));
    }

    #[test]
    fn extract_keywords_from_entries() {
        let entries = vec![
            make_entry(
                LogLevel::Error,
                LogCategory::ToolCall,
                "shell",
                "command_failed: run_git timed out",
                Value::Null,
            ),
            make_entry(
                LogLevel::Error,
                LogCategory::LlmCall,
                "gemini",
                "rate_limit exceeded for chat_with_system",
                Value::Null,
            ),
        ];
        let keywords = extract_search_keywords(&entries);
        assert!(!keywords.is_empty());
        // Should contain component names
        assert!(keywords.contains(&"shell".to_string()));
        assert!(keywords.contains(&"gemini".to_string()));
    }

    #[test]
    fn determine_paths_from_entries() {
        let entries = vec![make_entry(
            LogLevel::Error,
            LogCategory::ToolCall,
            "shell",
            "command failed",
            Value::Null,
        )];
        let paths = determine_search_paths(&entries);
        // Should include tool_call → src/tools/
        assert!(paths.iter().any(|p| p.contains("tools")));
        // Should always include agent
        assert!(paths.iter().any(|p| p.contains("agent")));
    }

    #[test]
    fn truncate_short_noop() {
        let s = "hello world";
        assert_eq!(truncate_if_needed(s, 100), s);
    }

    #[test]
    fn truncate_long() {
        let s = "a".repeat(200);
        let result = truncate_if_needed(&s, 50);
        assert!(result.len() < 200);
        assert!(result.contains("截断"));
    }
}
