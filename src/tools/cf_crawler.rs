// elfClaw: native cf-crawler tools — direct process invocation, no shell.
//
// Replaces the shell-templated SKILL.toml tools (web_scrape/web_crawl/web_login/
// web_health, formerly defined in 资料/skills/cf-crawler/SKILL.toml) that piped
// JSON through `sh`/`powershell` and repeatedly broke on bash-vs-PowerShell
// quoting/escaping differences — see dev_log.md for the multi-session debugging
// history (POSIX `\t`/`\c` escape corruption, double-workspace path bugs, etc).
//
// tokio::process::Command builds the child process argv array directly (Windows
// CreateProcess / POSIX execve), without any shell interpreting the string in
// between — so a JSON payload containing quotes, backslashes, or newlines is
// passed to cf-crawler.exe exactly as constructed. There is nothing to escape.
//
// The exe reads its Cloudflare Worker endpoint + auth token from the
// CF_CRAWLER_ENDPOINT / CF_CRAWLER_TOKEN environment variables (inherited from
// this process's environment — unlike the generic `shell` tool, this only ever
// runs one hardcoded, trusted binary with a fixed set of subcommands, so there is
// no arbitrary-command risk that would call for an env allowlist).

use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const HEALTH_TIMEOUT_SECS: u64 = 30;
const SCRAPE_TIMEOUT_SECS: u64 = 90;
const CRAWL_TIMEOUT_SECS: u64 = 120;
const LOGIN_TIMEOUT_SECS: u64 = 90;

const EXE_NAME: &str = "cf-crawler-win-x64.exe";

fn cf_crawler_exe_path(security: &SecurityPolicy) -> anyhow::Result<PathBuf> {
    if !cfg!(windows) {
        anyhow::bail!("cf-crawler 工具目前只支持 Windows（{EXE_NAME}）");
    }
    let path = security.workspace_dir.join("tools").join(EXE_NAME);
    if !path.exists() {
        anyhow::bail!(
            "cf-crawler 未安装：{} 不存在，请检查 workspace/tools/ 目录",
            path.display()
        );
    }
    Ok(path)
}

/// Run cf-crawler.exe with an optional `--json <payload>` argument. No shell involved.
async fn run_cf_crawler(
    security: &SecurityPolicy,
    subcommand: &str,
    payload: Option<&Value>,
    timeout_secs: u64,
) -> anyhow::Result<Value> {
    let exe = cf_crawler_exe_path(security)?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg(subcommand);
    if let Some(p) = payload {
        cmd.arg("--json").arg(p.to_string());
    }

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("cf-crawler {subcommand} 超时（{timeout_secs}秒）"))?
        .map_err(|e| anyhow::anyhow!("无法启动 cf-crawler: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // cf-crawler's CLI always writes its final structured result as a JSON
    // object on stdout — but on failure it first writes a pino logger line
    // (`{"level":50,...,"msg":"command failed"}`) to stdout too (verified by
    // running cf-crawler-win-x64.exe locally with no Worker reachable), so
    // stdout can contain more than one JSON line. Scan from the last line
    // backwards for the first one that looks like a result object (has a
    // "success" or "ok" field — pino log lines never do).
    let result_line = stdout.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        if v.get("success").is_some() || v.get("ok").is_some() {
            Some(v)
        } else {
            None
        }
    });

    match result_line {
        Some(v) => Ok(v),
        None => {
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            anyhow::bail!(
                "cf-crawler {subcommand} 执行失败（退出码 {:?}）: {detail}",
                output.status.code()
            );
        }
    }
}

/// Convert a parsed cf-crawler JSON response into a ToolResult.
/// `health` uses an `ok` field; every other command uses `success`.
fn result_from_json(v: Value) -> ToolResult {
    let success = v
        .get("success")
        .and_then(Value::as_bool)
        .or_else(|| v.get("ok").and_then(Value::as_bool))
        .unwrap_or(false);
    let error = if success {
        None
    } else {
        v.get("error")
            .and_then(Value::as_str)
            .map(std::string::ToString::to_string)
    };
    ToolResult {
        success,
        output: v.to_string(),
        error,
    }
}

// ── web_health ───────────────────────────────────────────────────────────────

pub struct WebHealthTool {
    security: Arc<SecurityPolicy>,
}

impl WebHealthTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for WebHealthTool {
    fn name(&self) -> &str {
        "web_health"
    }

    fn description(&self) -> &str {
        "检查 cf-crawler Cloudflare Worker 是否正常运行。返回版本号和 Browser Rendering 状态。\
         报错先调用本工具确认 Worker 是否在线，再排查其他 cf-crawler 工具的问题。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        let v = run_cf_crawler(&self.security, "health", None, HEALTH_TIMEOUT_SECS).await?;
        Ok(result_from_json(v))
    }
}

// ── web_scrape ───────────────────────────────────────────────────────────────

pub struct WebScrapeTool {
    security: Arc<SecurityPolicy>,
}

impl WebScrapeTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for WebScrapeTool {
    fn name(&self) -> &str {
        "web_scrape"
    }

    fn description(&self) -> &str {
        "抓取单个网页并提取内容。返回标题、markdown 正文和链接。支持自动反爬绕过\
         （Cloudflare Browser Rendering）。如果调用报错，先用 web_health 确认 Worker \
         状态。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Target URL to scrape (required)"},
                "goal": {"type": "string", "description": "Extraction goal description (required)"},
                "mode": {
                    "type": "string",
                    "enum": ["article", "listing", "raw", "feed", "screenshot"],
                    "description": "Extraction mode (optional, default article)"
                },
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "edge_fetch", "edge_browser"],
                    "description": "Fetch strategy (optional, default auto)"
                },
                "device_type": {
                    "type": "string",
                    "enum": ["desktop", "mobile"],
                    "description": "Device type to emulate (optional)"
                },
                "session_id": {"type": "string", "description": "Reuse a login session ID (optional)"}
            },
            "required": ["url", "goal"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if args
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("url 参数不能为空");
        }
        if args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("goal 参数不能为空");
        }
        let v = run_cf_crawler(
            &self.security,
            "scrape-page",
            Some(&args),
            SCRAPE_TIMEOUT_SECS,
        )
        .await?;
        Ok(result_from_json(v))
    }
}

// ── web_crawl ────────────────────────────────────────────────────────────────

pub struct WebCrawlTool {
    security: Arc<SecurityPolicy>,
}

impl WebCrawlTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for WebCrawlTool {
    fn name(&self) -> &str {
        "web_crawl"
    }

    fn description(&self) -> &str {
        "从种子 URL 开始批量爬取网站。使用 BFS 算法，支持限速和去重。返回页面数组，\
         每页含标题、markdown 和链接。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "seed_url": {"type": "string", "description": "Starting URL for crawling (required)"},
                "goal": {"type": "string", "description": "Crawling goal description (required)"},
                "scope": {
                    "type": "string",
                    "enum": ["same_host", "same_path", "custom"],
                    "description": "Crawl scope (optional)"
                },
                "max_pages": {"type": "integer", "description": "Maximum pages to crawl, default 5 (optional)"},
                "depth": {"type": "integer", "description": "Maximum crawl depth, default 2 (optional)"},
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "edge_fetch", "edge_browser"],
                    "description": "Fetch strategy (optional, default auto)"
                },
                "allowed_patterns": {
                    "type": "string",
                    "description": "URL patterns for custom scope, comma-separated (optional)"
                }
            },
            "required": ["seed_url", "goal"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if args
            .get("seed_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("seed_url 参数不能为空");
        }
        if args
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("goal 参数不能为空");
        }
        let v = run_cf_crawler(
            &self.security,
            "crawl-site",
            Some(&args),
            CRAWL_TIMEOUT_SECS,
        )
        .await?;
        Ok(result_from_json(v))
    }
}

// ── web_login ────────────────────────────────────────────────────────────────

pub struct WebLoginTool {
    security: Arc<SecurityPolicy>,
}

impl WebLoginTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self { security }
    }
}

#[async_trait]
impl Tool for WebLoginTool {
    fn name(&self) -> &str {
        "web_login"
    }

    fn description(&self) -> &str {
        "在网站上执行登录流程。创建可复用的持久会话，之后 web_scrape 可通过 session_id \
         使用该会话。需要 Cloudflare Browser Rendering。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Login page URL (required)"},
                "steps": {
                    "type": "array",
                    "description": "Login action steps [{action,selector,value}] (required)",
                    "items": {"type": "object"}
                },
                "session_id": {"type": "string", "description": "Session name for reuse (optional)"}
            },
            "required": ["url", "steps"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        if args
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .is_empty()
        {
            anyhow::bail!("url 参数不能为空");
        }
        let steps_len = args
            .get("steps")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        if steps_len == 0 {
            anyhow::bail!("steps 参数不能为空");
        }
        let v = run_cf_crawler(&self.security, "login", Some(&args), LOGIN_TIMEOUT_SECS).await?;
        Ok(result_from_json(v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_security(workspace_dir: std::path::PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            workspace_dir,
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn cf_crawler_exe_path_errors_when_exe_missing() {
        let dir = tempfile::tempdir().unwrap();
        let security = test_security(dir.path().to_path_buf());
        let err = cf_crawler_exe_path(&security).unwrap_err();
        assert!(err.to_string().contains("未安装"));
    }

    #[test]
    fn cf_crawler_exe_path_resolves_under_workspace_tools_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::write(tools_dir.join(EXE_NAME), b"stub").unwrap();
        let security = test_security(dir.path().to_path_buf());
        let path = cf_crawler_exe_path(&security).unwrap();
        assert_eq!(path, tools_dir.join(EXE_NAME));
    }

    #[test]
    fn result_from_json_reads_success_field() {
        let v = json!({"success": true, "title": "hello"});
        let r = result_from_json(v);
        assert!(r.success);
        assert!(r.error.is_none());
    }

    #[test]
    fn result_from_json_falls_back_to_ok_field_for_health() {
        let v = json!({"ok": true, "command": "health"});
        let r = result_from_json(v);
        assert!(r.success);
    }

    #[test]
    fn result_from_json_surfaces_error_message_on_failure() {
        let v = json!({"success": false, "error": "boom", "hint": "..."});
        let r = result_from_json(v);
        assert!(!r.success);
        assert_eq!(r.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn web_scrape_rejects_empty_url() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebScrapeTool::new(test_security(dir.path().to_path_buf()));
        let err = tool
            .execute(json!({"url": "", "goal": "test"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("url"));
    }

    #[tokio::test]
    async fn web_scrape_rejects_missing_goal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebScrapeTool::new(test_security(dir.path().to_path_buf()));
        let err = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("goal"));
    }

    #[tokio::test]
    async fn web_crawl_rejects_empty_seed_url() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebCrawlTool::new(test_security(dir.path().to_path_buf()));
        let err = tool
            .execute(json!({"seed_url": "", "goal": "test"}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("seed_url"));
    }

    #[tokio::test]
    async fn web_login_rejects_empty_steps() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebLoginTool::new(test_security(dir.path().to_path_buf()));
        let err = tool
            .execute(json!({"url": "https://example.com", "steps": []}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("steps"));
    }

    #[tokio::test]
    async fn web_health_errors_clearly_when_exe_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebHealthTool::new(test_security(dir.path().to_path_buf()));
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("未安装"));
    }

    // elfClaw: local-only manual verification against the real exe. Ignored by
    // default (path is this dev machine's, not present in CI); run with
    // `cargo test --lib -- --ignored cf_crawler::tests::manual_web_health_against_real_exe`.
    // Confirms the mixed pino-log-line + result-line stdout parsing actually
    // works against the unmodified binary, not just a hand-written fixture.
    #[tokio::test]
    #[ignore]
    async fn manual_web_health_against_real_exe() {
        let real_exe = std::path::Path::new(r"C:\Dev\cf-crawler\release\cf-crawler-win-x64.exe");
        assert!(
            real_exe.exists(),
            "real cf-crawler exe not found for manual test"
        );
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::copy(real_exe, tools_dir.join(EXE_NAME)).unwrap();
        let tool = WebHealthTool::new(test_security(dir.path().to_path_buf()));
        // No CF_CRAWLER_ENDPOINT/TOKEN set — expect a clean success:false result
        // (parsed past the pino error log line), not a "non-JSON stdout" failure.
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("\"success\":false"));
    }

    /// elfClaw: manual-only. Verifies a URL/goal containing quotes, `&`, and
    /// backslashes survives argv passing intact — this is the exact class of
    /// bug (bash/PowerShell re-interpreting `\t`, `\c`, quotes) that made the
    /// old SKILL.toml shell-templated tools unreliable (see dev_log.md).
    /// tokio::process::Command builds argv directly, so nothing should need
    /// escaping; this asserts we reach the network layer (ECONNREFUSED) rather
    /// than a JSON parse error on the cf-crawler side.
    #[tokio::test]
    #[ignore]
    async fn manual_web_scrape_special_chars_survive_argv_against_real_exe() {
        let real_exe = std::path::Path::new(r"C:\Dev\cf-crawler\release\cf-crawler-win-x64.exe");
        assert!(
            real_exe.exists(),
            "real cf-crawler exe not found for manual test"
        );
        let dir = tempfile::tempdir().unwrap();
        let tools_dir = dir.path().join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
        std::fs::copy(real_exe, tools_dir.join(EXE_NAME)).unwrap();
        let tool = WebScrapeTool::new(test_security(dir.path().to_path_buf()));
        let result = tool
            .execute(json!({
                "url": "https://example.com/path?a=1&b=2",
                "goal": "test \"quotes\", \\backslashes\\ and \ttabs"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("ECONNREFUSED"),
            "expected to reach the network layer, got: {}",
            result.output
        );
    }
}
