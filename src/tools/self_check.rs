// elfClaw: self_check tool — autonomous diagnostics with isolated analysis
//
// Architecture (v0.3.2):
// - action="analyze" (default): collect + isolated agent::run() + save report.
//   Runs analysis in a separate process context with full 1M token window,
//   avoiding the 8K truncation problem in shared conversation history.
// - action="collect": sync source → collect logs → search error keywords →
//   read key files → return structured JSON (for direct inspection).
// - action="save_report": receive report text → write to homework/.

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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ── Limits ──────────────────────────────────────────────────────────────────

const MAX_SEARCH_RESULT_CHARS: usize = 3_000;
const MAX_FILE_READ_CHARS: usize = 4_000;
const SYNC_TIMEOUT_SECS: u64 = 120;

/// Prevent concurrent/recursive analyze calls.
static ANALYZE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// ── elfClaw: SelfCheckGate — hard runtime gate for self_check + check_logs ──
// Default: disabled. Only user `/selfcheck <prompt>` command opens the gate.
// Prevents LLM from self-initiating diagnostic loops that pollute context/memory.
static SELF_CHECK_ENABLED: AtomicBool = AtomicBool::new(false);
static SELF_CHECK_PROMPT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Global gate controlling self_check + check_logs tool availability.
/// Default: closed (disabled). Opened by user command, auto-closed after completion.
pub struct SelfCheckGate;

impl SelfCheckGate {
    /// Open the gate with a user-provided prompt describing what to check.
    pub fn open(prompt: &str) {
        *SELF_CHECK_PROMPT.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(prompt.to_string());
        SELF_CHECK_ENABLED.store(true, Ordering::SeqCst);
    }

    /// Close the gate and clear the prompt.
    pub fn close() {
        SELF_CHECK_ENABLED.store(false, Ordering::SeqCst);
        *SELF_CHECK_PROMPT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Check if the gate is currently open.
    pub fn is_open() -> bool {
        SELF_CHECK_ENABLED.load(Ordering::SeqCst)
    }

    /// Take the stored prompt (returns and clears it).
    pub fn take_prompt() -> Option<String> {
        SELF_CHECK_PROMPT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}
/// Maximum wall-clock time for the isolated analysis agent.
const ANALYZE_TIMEOUT_SECS: u64 = 300;
/// Maximum agentic iterations for the analysis agent.
const ANALYZE_MAX_ITERATIONS: usize = 15;
/// Max chars of the analysis report returned to the main conversation.
const REPORT_SUMMARY_CHARS: usize = 800;

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
            .join("github")
            .join(repo_id)
            .join("Cargo.toml");
        marker.exists()
    }

    /// Collect diagnostic data: sync sources, gather logs, search code, read files.
    /// Returns structured JSON — zero LLM calls.
    async fn collect(&self, since_minutes: u64) -> anyhow::Result<ToolResult> {
        // ── Phase 0: Environment info ─────────────────────────────────
        let has_rg = which::which("rg").is_ok();
        let has_grep = which::which("grep").is_ok();
        let has_findstr = which::which("findstr").is_ok();

        let environment = json!({
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "hostname": hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "unknown".into()),
            "search_backend": if has_rg { "rg" } else if has_grep { "grep" }
                else if has_findstr { "findstr" } else { "none" },
            "cli_tools": {
                "rg": has_rg, "grep": has_grep, "findstr": has_findstr,
                "git": which::which("git").is_ok(),
            },
        });

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

        // ── Step 2: Collect structured logs (error + warn combined) ────
        let error_entries =
            elfclaw_log::query_recent(80, Some("error"), None, Some(since_minutes));
        let warn_entries =
            elfclaw_log::query_recent(30, Some("warn"), None, Some(since_minutes));
        let entries: Vec<LogEntry> = error_entries.into_iter().chain(warn_entries).collect();

        // Convert log entries to JSON with source_hint classification
        let logs_json: Vec<Value> = entries
            .iter()
            .map(|e| {
                let source_hint = match e.category {
                    elfclaw_log::LogCategory::ToolCall => "user-triggered tool execution",
                    elfclaw_log::LogCategory::CronJob => "scheduled background task",
                    elfclaw_log::LogCategory::LlmCall => "LLM API interaction",
                    elfclaw_log::LogCategory::ChannelMessage => "user chat message handling",
                    elfclaw_log::LogCategory::Heartbeat => "heartbeat periodic task",
                    elfclaw_log::LogCategory::AgentLifecycle => "agent start/stop lifecycle",
                    elfclaw_log::LogCategory::WorkerStatus => "worker process status",
                    elfclaw_log::LogCategory::System => "system/daemon lifecycle",
                };
                json!({
                    "level": e.level.as_str(),
                    "category": e.category.as_str(),
                    "source_hint": source_hint,
                    "component": &e.component,
                    "message": &e.message,
                    "timestamp": &e.timestamp,
                    "details": &e.details,
                })
            })
            .collect();

        // ── Collect INFO logs as independent context (does not affect entries empty check) ──
        let info_entries =
            elfclaw_log::query_recent(30, Some("info"), None, Some(since_minutes));
        let info_json: Vec<Value> = info_entries
            .iter()
            .map(|e| {
                let source_hint = match e.category {
                    elfclaw_log::LogCategory::ToolCall => "user-triggered tool execution",
                    elfclaw_log::LogCategory::CronJob => "scheduled background task",
                    elfclaw_log::LogCategory::LlmCall => "LLM API interaction",
                    elfclaw_log::LogCategory::ChannelMessage => "user chat message handling",
                    elfclaw_log::LogCategory::Heartbeat => "heartbeat periodic task",
                    elfclaw_log::LogCategory::AgentLifecycle => "agent start/stop lifecycle",
                    elfclaw_log::LogCategory::WorkerStatus => "worker process status",
                    elfclaw_log::LogCategory::System => "system/daemon lifecycle",
                };
                json!({
                    "level": "info",
                    "category": e.category.as_str(),
                    "source_hint": source_hint,
                    "component": &e.component,
                    "message": &e.message,
                    "timestamp": &e.timestamp,
                })
            })
            .collect();

        let mode = if has_source_code { "full" } else { "log_only" };

        // If no logs and no source, return early
        if entries.is_empty() && !has_source_code {
            return Ok(ToolResult {
                success: true,
                output: json!({
                    "environment": environment,
                    "mode": mode,
                    "sync_status": sync_status.join("; "),
                    "source_base_path": "github/elfclaw",
                    "usage_hint": "如需查看源码，使用 file_read(path: \"github/elfclaw/src/xxx.rs\")。禁止使用 shell/git 命令。",
                    "logs": [],
                    "info_context": info_json,
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
                    "environment": environment,
                    "mode": mode,
                    "sync_status": sync_status.join("; "),
                    "source_base_path": "github/elfclaw",
                    "usage_hint": "如需查看源码，使用 file_read(path: \"github/elfclaw/src/xxx.rs\")。禁止使用 shell/git 命令。",
                    "logs": [],
                    "info_context": info_json,
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
                    let relative_path = format!(
                        "github/elfclaw/{}",
                        search_path
                    );

                    if let Ok(r) = self
                        .content_search
                        .execute(json!({
                            "pattern": keyword,
                            "path": &relative_path,
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
                let relative_path = format!(
                    "github/elfclaw/{}",
                    key_file
                );

                if let Ok(r) = self
                    .file_read
                    .execute(json!({
                        "path": &relative_path,
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
                .join("github/elfclaw/src");
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
            "environment": environment,
            "mode": mode,
            "sync_status": sync_status.join("; "),
            "source_base_path": "github/elfclaw",
            "usage_hint": "如需查看源码，使用 file_read(path: \"github/elfclaw/src/xxx.rs\")。禁止使用 shell/git 命令。",
            "logs": logs_json,
            "info_context": info_json,
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

    /// Run full autonomous analysis: collect → isolated agent analysis → save report.
    async fn analyze(&self, since_minutes: u64) -> anyhow::Result<ToolResult> {
        // Prevent concurrent/recursive analyze calls
        if ANALYZE_IN_PROGRESS
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("self_check analyze 已在运行中".into()),
            });
        }
        let result = self.analyze_inner(since_minutes).await;
        ANALYZE_IN_PROGRESS.store(false, Ordering::SeqCst);
        result
    }

    async fn analyze_inner(&self, since_minutes: u64) -> anyhow::Result<ToolResult> {
        // 1. Collect full diagnostic data (internal call, not truncated by history)
        let collect_result = self.collect(since_minutes).await?;
        if !collect_result.success {
            return Ok(collect_result);
        }

        // elfClaw: inject user's focus prompt from SelfCheckGate if available
        let user_focus = SelfCheckGate::take_prompt().unwrap_or_default();
        let focus_section = if user_focus.is_empty() {
            String::new()
        } else {
            format!(
                "\n## 用户重点检查方向\n\
                 用户要求重点检查以下方面，请优先分析：\n{user_focus}\n"
            )
        };

        // 2. Build analysis prompt with anti-hallucination rules
        let prompt = format!(
            "你是 elfClaw 系统的自检分析器。\n\
             以下是自动收集的完整系统诊断数据 JSON。\n\
             {focus_section}\n\
             ## 分析要求\n\
             1. 先阅读 environment 部分，了解运行环境（OS、架构、可用工具）\n\
             2. 使用 source_hint 字段区分日志来源：\n\
                - \"user-triggered tool execution\" = 用户操作触发的工具调用\n\
                - \"scheduled background task\" = 定时任务\n\
                - \"LLM API interaction\" = LLM 调用\n\
                - \"system/daemon lifecycle\" = 系统启停\n\
                用户操作记录（如 shell 命令失败）不是系统缺陷，应归类为「用户操作记录」\n\
             3. 分析 logs 中的错误，找出根因和模式\n\
             4. 利用 info_context 字段了解系统运行上下文：\n\
                - agent_lifecycle：agent 启停、MCP 连接状态\n\
                - cron_job：定时任务触发时间和结果\n\
                - tool_call：成功的工具调用记录\n\
                - system：daemon 生命周期事件\n\
                INFO 日志不是问题本身，但能帮助理解问题发生时的系统状态\n\
             5. 检查 search_results 和 key_files 中的相关代码\n\
             6. 如需查看更多源码，使用 file_read(path: \"github/elfclaw/src/xxx.rs\")\n\
             7. 最多额外查看 5 个文件，不要过度探索\n\n\
             ## 反编造规则（必须遵守）\n\
             - 只报告数据中有直接证据的问题，禁止推测或编造不存在的错误\n\
             - 每个问题必须标注 timestamp 和 component\n\
             - 禁止引用数据中没有的版本号、数字或统计\n\
             - 禁止使用「你」「您」等个人称呼，使用「系统」「运行环境」等客观表述\n\
             - 如果日志为空或无异常，直接报告「系统正常」即可\n\n\
             ## 报告格式（中文）\n\
             1. **运行环境概况**（OS、架构、可用工具）\n\
             2. **系统健康状态**（正常/有问题/严重）\n\
             3. **发现的问题**（按严重程度排列，每条标注 timestamp + component）\n\
                - 🔴 严重（影响核心功能）\n\
                - 🟡 中等（影响体验但不致命）\n\
                - 🔵 信息（用户操作记录，非系统缺陷）\n\
             4. **根因分析**（仅基于日志和代码证据）\n\
             5. **修复建议**（具体操作步骤）\n\n\
             ## 禁止使用的工具\n\
             - shell — 防止产生副作用\n\
             - git_operations — 防止修改仓库状态\n\
             - content_search — Windows 兼容性问题，使用 file_read 替代\n\
             - self_check — 防止递归调用\n\n\
             直接输出报告文本。\n\n\
             ---\n\
             诊断数据：\n{}",
            collect_result.output
        );

        // 3. Run analysis in isolated context (uses worker_model — self_check prompt contains all
        //    diagnostic data explicitly; worker model is sufficient for log analysis and avoids
        //    competing with interactive user sessions for the primary model's rate limit)
        let report = tokio::time::timeout(
            Duration::from_secs(ANALYZE_TIMEOUT_SECS),
            crate::agent::loop_::run(
                (*self.config).clone(),
                Some(prompt),
                None,   // provider: config default
                None,   // model: config default
                0.3,    // low temperature for analytical precision
                vec![],
                false,
                Some(ANALYZE_MAX_ITERATIONS),
                crate::agent::RunContext::Background, // elfClaw: use worker model; avoids rate limit
                                                      // competition with interactive user sessions
                None, // elfClaw: no tool filtering for self_check
            ),
        )
        .await;

        match report {
            Ok(Ok(report_text)) => {
                // 4. Save full report
                let save_result = self.save_report(&report_text).await?;

                // 5. Return summary to main conversation
                let summary = format!(
                    "自检分析完成。{}\n\n报告摘要：\n{}",
                    save_result.output,
                    truncate_if_needed(&report_text, REPORT_SUMMARY_CHARS)
                );
                Ok(ToolResult {
                    success: true,
                    output: summary,
                    error: None,
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("分析失败: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("分析超时（{}秒）", ANALYZE_TIMEOUT_SECS)),
            }),
        }
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
        "Self-check diagnostic tool. \
         CONFIRMATION REQUIRED: Before calling this tool, you MUST ask the user for explicit \
         confirmation. Only call if the user's current message clearly and explicitly requests a \
         self-check (e.g., '请执行自检', 'run self-check', '健康检查'). \
         Do NOT call automatically in response to errors, cron failures, tool failures, or \
         system events — those are not triggers for self-check. \
         Actions: 'analyze' (default — collects diagnostics, runs analysis via worker model, saves \
         report, returns summary), 'collect' (raw JSON data), 'save_report' (save text to file)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["analyze", "collect", "save_report"],
                    "description": "Action: 'analyze' = full autonomous diagnostics (default), 'collect' = raw JSON data, 'save_report' = save report text",
                    "default": "analyze"
                },
                "since_minutes": {
                    "type": "integer",
                    "description": "Log lookback window in minutes. Default: 60. Used with 'analyze' and 'collect'.",
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
        // ── elfClaw: hard gate — only runs when user explicitly opens via /selfcheck ──
        // elfClaw: return success:true so weak models don't try to "fix" the failure
        // by attempting manual diagnosis with shell/file_read/glob_search
        if !SelfCheckGate::is_open() {
            return Ok(ToolResult {
                success: true,
                output: "self_check is currently disabled. \
                         Please tell the user: 自检功能当前未启用。如需运行自检，请发送 /selfcheck 命令。"
                    .into(),
                error: None,
            });
        }

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
            .unwrap_or("analyze");

        match action {
            "analyze" => {
                let since_minutes = args
                    .get("since_minutes")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60);
                self.analyze(since_minutes).await
            }
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
                    "Unknown action '{other}'. Use 'analyze', 'collect', or 'save_report'."
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
