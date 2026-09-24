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
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Semaphore;

const HEALTH_TIMEOUT_SECS: u64 = 30;
const SCRAPE_TIMEOUT_SECS: u64 = 90;
const CRAWL_TIMEOUT_SECS: u64 = 120;
const LOGIN_TIMEOUT_SECS: u64 = 90;

const EXE_NAME: &str = "cf-crawler-win-x64.exe";

/// elfClaw 2026-09-24: at most this many cf-crawler runs at once (health
/// excluded). Cloudflare Browser Rendering on the free plan allows only a few
/// concurrent browsers and a few new browsers per minute; the news worker
/// fires several `web_scrape` calls in parallel, which used to trip
/// "Unable to create new browser: code: 429: Rate limit exceeded".
const MAX_CONCURRENT_RUNS: usize = 2;
static RUN_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_RUNS));

/// Waits before re-running a scrape that hit the Browser Rendering rate limit
/// (the limit is per minute).
const BROWSER_RATE_LIMIT_RETRY_WAITS_SECS: [u64; 2] = [20, 40];

/// Shown to the model when the Browser Rendering limit persists after the
/// retries. The "CF浏览器限流" wording is what `news::record_results` treats as a
/// transient failure that does not count toward banning the source.
const BROWSER_RATE_LIMIT_MESSAGE: &str =
    "CF浏览器限流：Cloudflare 免费计划每分钟能新开的浏览器数量有限，\
     已自动等待重试仍然受限。这不是该网站失效——news_report 时 reason 请写「CF浏览器限流」\
     （不计入失败次数）。可以稍后再试，或对不需要浏览器的源改用 strategy=edge_fetch。";

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
    let _slot = if subcommand == "health" {
        None
    } else {
        Some(
            RUN_SLOTS
                .acquire()
                .await
                .map_err(|e| anyhow::anyhow!("cf-crawler 并发控制异常: {e}"))?,
        )
    };
    let mut cmd = tokio::process::Command::new(&exe);
    // elfClaw 2026-09-24: cf-crawler writes relative output paths (screenshots
    // to `homework/screenshots/`, `persist_path`) against its working directory.
    // Without this they landed next to the daemon's config dir, outside the
    // workspace, where the agent's file tools cannot reach them.
    cmd.current_dir(&security.workspace_dir);
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

    match parse_result_line(&stdout) {
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

/// Find cf-crawler's structured result in its stdout.
///
/// cf-crawler's CLI writes its final result as a JSON object on stdout, but
/// it also writes pino logger lines (`{"level":30,...,"msg":"scrape-page
/// completed"}`) to stdout, so stdout holds more than one JSON line. Scan from
/// the last line backwards for the first one that looks like a result object
/// (has a "success" or "ok" field — pino log lines never do).
///
/// elfClaw 2026-09-24: only the leading JSON value of each line is parsed and
/// anything after it is ignored. cf-crawler 0.3.x ends the result line of
/// scrape-page/crawl/login with a literal backslash + `n` instead of a
/// newline (`src/cli/index.ts`: `` `${JSON.stringify(result)}\\n` ``), so a
/// whole-line parse rejected every *successful* scrape and the tool reported
/// "执行失败（退出码 Some(0)）" — the news worker then hit loop detection after
/// four such "failures" in a row. Only `help` and the error path used a real
/// newline, so failures parsed fine and successes did not. Fixed at the source
/// in cf-crawler commit `ead4483`; this parser stays tolerant so older exes
/// keep working.
fn parse_result_line(stdout: &str) -> Option<Value> {
    stdout.lines().rev().find_map(|line| {
        let v = serde_json::Deserializer::from_str(line.trim())
            .into_iter::<Value>()
            .next()?
            .ok()?;
        if v.get("success").is_some() || v.get("ok").is_some() {
            Some(v)
        } else {
            None
        }
    })
}

/// True when cf-crawler reports that Cloudflare refused to start a browser
/// because of the Browser Rendering rate limit (surfaced in `error` since
/// cf-crawler 0.3.2; older builds only reported `render_error`).
fn is_browser_rate_limited(v: &Value) -> bool {
    let failed = v.get("success").and_then(Value::as_bool) == Some(false)
        || v.get("ok").and_then(Value::as_bool) == Some(false);
    if !failed {
        return false;
    }
    let error = v.get("error").and_then(Value::as_str).unwrap_or_default();
    let lower = error.to_ascii_lowercase();
    lower.contains("rate limit exceeded") || (lower.contains("browser") && lower.contains("429"))
}

/// Run a browser-using cf-crawler command (scrape-page / login), waiting and
/// re-running it while Cloudflare reports the Browser Rendering rate limit.
/// If the limit persists, returns a failed result carrying
/// `BROWSER_RATE_LIMIT_MESSAGE`.
async fn run_with_browser_retry(
    security: &SecurityPolicy,
    subcommand: &str,
    args: &Value,
    timeout_secs: u64,
) -> anyhow::Result<ToolResult> {
    let mut waits = BROWSER_RATE_LIMIT_RETRY_WAITS_SECS.iter();
    loop {
        let v = run_cf_crawler(security, subcommand, Some(args), timeout_secs).await?;
        if !is_browser_rate_limited(&v) {
            return Ok(result_from_json(v));
        }
        let Some(wait) = waits.next() else {
            return Ok(ToolResult {
                success: false,
                output: v.to_string(),
                error: Some(BROWSER_RATE_LIMIT_MESSAGE.to_string()),
            });
        };
        tracing::warn!(
            subcommand,
            wait_secs = wait,
            "cf-crawler hit the Browser Rendering rate limit, retrying"
        );
        tokio::time::sleep(Duration::from_secs(*wait)).await;
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
        "抓取单个网页并提取内容。返回标题、markdown 正文和链接。strategy：auto（默认，\
         先直接抓，被拦截再自动用 Cloudflare 浏览器渲染）、edge_fetch（只直接抓，最快）、\
         edge_browser（直接用浏览器渲染，适合需要 JavaScript 的页面）、paywall_bypass\
         （付费墙绕过，会尝试网页存档等来源）。mode=screenshot 时截图保存在 workspace 的 \
         homework/screenshots/ 下，返回的 screenshot_path 可直接用于文件或发送工具。\
         如果调用报错，先用 web_health 确认 Worker 状态。"
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
                    "enum": ["auto", "edge_fetch", "edge_browser", "paywall_bypass"],
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
        run_with_browser_retry(&self.security, "scrape-page", &args, SCRAPE_TIMEOUT_SECS).await
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
         每页含标题、markdown 和链接。默认最多 20 页、深度 2，只爬同一域名。"
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
                "max_pages": {"type": "integer", "description": "Maximum pages to crawl, 1-200, default 20 (optional)"},
                "depth": {"type": "integer", "description": "Maximum crawl depth, 0-6, default 2 (optional)"},
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "edge_fetch", "edge_browser"],
                    "description": "Fetch strategy (optional, default auto)"
                },
                "include_patterns": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Only crawl URLs matching these patterns (optional; used with scope=custom)"
                },
                "exclude_patterns": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Skip URLs matching these patterns (optional)"
                },
                "session_id": {"type": "string", "description": "Reuse a web_login session ID (optional)"}
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
        "用 Cloudflare 浏览器登录网站并保存会话（cookie）。之后 web_scrape / web_crawl \
         传同一个 session_id 就能带着登录状态抓取。需要登录页上用户名、密码输入框的 CSS \
         选择器。账号密码只在用户明确提供时使用，不要自己编造或复述到聊天里。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Name for the saved session, reused by web_scrape/web_crawl (required)"},
                "login_url": {"type": "string", "description": "Login page URL (required)"},
                "credentials": {
                    "type": "object",
                    "description": "Login form fields (required)",
                    "properties": {
                        "username_field": {"type": "string", "description": "CSS selector of the username input, e.g. #username"},
                        "username": {"type": "string"},
                        "password_field": {"type": "string", "description": "CSS selector of the password input, e.g. input[type=password]"},
                        "password": {"type": "string"}
                    },
                    "required": ["username_field", "username", "password_field", "password"]
                },
                "submit_selector": {"type": "string", "description": "CSS selector of the submit button (optional; Enter is pressed otherwise)"},
                "success_url_contains": {"type": "string", "description": "Text the URL contains after a successful login (optional)"}
            },
            "required": ["session_id", "login_url", "credentials"]
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let non_empty = |v: Option<&Value>| {
            v.and_then(Value::as_str)
                .is_some_and(|s| !s.trim().is_empty())
        };
        for field in ["session_id", "login_url"] {
            if !non_empty(args.get(field)) {
                anyhow::bail!("{field} 参数不能为空");
            }
        }
        let credentials = args.get("credentials");
        for field in ["username_field", "username", "password_field", "password"] {
            if !non_empty(credentials.and_then(|c| c.get(field))) {
                anyhow::bail!("credentials.{field} 参数不能为空");
            }
        }
        run_with_browser_retry(&self.security, "login", &args, LOGIN_TIMEOUT_SECS).await
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
    async fn web_login_rejects_blank_credential_fields() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebLoginTool::new(test_security(dir.path().to_path_buf()));
        let err = tool
            .execute(
                json!({"session_id": "s", "login_url": "https://example.com/login",
                            "credentials": {"username_field": "#u", "username": "zeroclaw_user",
                                            "password_field": "#p", "password": " "}}),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("credentials.password"));
    }

    #[tokio::test]
    async fn web_health_errors_clearly_when_exe_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WebHealthTool::new(test_security(dir.path().to_path_buf()));
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("未安装"));
    }

    #[test]
    fn parse_result_line_accepts_literal_backslash_n_after_json() {
        // Exact shape captured from cf-crawler-win-x64.exe 0.3.1 on K6
        // (scrape-page, exit code 0): pino info line, then the result line
        // terminated by the two characters `\` `n`.
        let stdout = concat!(
            r#"{"level":30,"time":1790228889563,"pid":6184,"hostname":"K6","name":"cf-crawler","url":"https://www.v2ex.com/index.xml","strategy":"edge_fetch","msg":"scrape-page completed"}"#,
            "\n",
            r#"{"success":true,"strategy_used":"edge_fetch","final_url":"https://www.v2ex.com/index.xml","title":"V2EX","markdown":"line one\nline two","meta":{"retries":0,"cache_hit":false}}\n"#,
        );
        let v = parse_result_line(stdout).expect("result line should parse");
        assert_eq!(v["success"], json!(true));
        assert_eq!(v["title"], json!("V2EX"));
        assert_eq!(v["markdown"], json!("line one\nline two"));
        let result = result_from_json(v);
        assert!(result.success);
    }

    #[test]
    fn browser_rate_limit_is_detected_only_on_failures_with_that_error() {
        assert!(is_browser_rate_limited(&json!({
            "success": false,
            "error": "Error: Unable to create new browser: code: 429: message: Rate limit exceeded"
        })));
        assert!(!is_browser_rate_limited(
            &json!({"success": false, "error": "HTTP 403 Forbidden"})
        ));
        assert!(!is_browser_rate_limited(
            &json!({"success": false, "anti_bot_signals": ["render_error"]})
        ));
        assert!(!is_browser_rate_limited(
            &json!({"success": true, "error": "Rate limit exceeded"})
        ));
        assert!(BROWSER_RATE_LIMIT_MESSAGE.contains("CF浏览器限流"));
        // login reports with `ok` instead of `success`
        assert!(is_browser_rate_limited(&json!({
            "ok": false,
            "error": "Error: Unable to create new browser: code: 429: message: Rate limit exceeded"
        })));
    }

    #[test]
    fn web_login_schema_matches_cf_crawler_login_input() {
        let dir = tempfile::tempdir().unwrap();
        let schema = WebLoginTool::new(test_security(dir.path().to_path_buf())).parameters_schema();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(required, vec!["session_id", "login_url", "credentials"]);
        for field in ["username_field", "username", "password_field", "password"] {
            assert!(schema["properties"]["credentials"]["properties"]
                .get(field)
                .is_some());
        }
    }

    #[test]
    fn parse_result_line_still_handles_clean_output_and_ignores_log_lines() {
        let stdout = concat!(
            r#"{"level":50,"msg":"command failed"}"#,
            "\n",
            r#"{"success":false,"error":"ECONNREFUSED"}"#,
            "\n",
        );
        let v = parse_result_line(stdout).unwrap();
        assert_eq!(v["success"], json!(false));

        assert!(parse_result_line(r#"{"level":30,"msg":"only a log line"}"#).is_none());
        assert!(parse_result_line("not json at all").is_none());
        assert!(parse_result_line("").is_none());
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
