//! elfClaw 2026-09-25: fetch pages with the machine's own Chrome.
//!
//! Some sources only exist as JavaScript-built pages (venue calendars, shop
//! rosters), and some serve a human check that a plain HTTP client never gets
//! past. Both are handled by driving the real Chrome already installed on the
//! box, against a persistent profile: whatever session the owner established
//! by hand (`local-browser login`, run from an interactive scheduled task) is
//! reused on later runs.
//!
//! Measured 2026-09-25: a headless Chrome is refused by sites that serve the
//! same stock Chrome fine when it has a real window, so the helper runs headed
//! with the window parked off-desktop. Nothing here spoofs a fingerprint,
//! rotates addresses or answers a challenge — when a site still refuses, the
//! per-URL result is `challenge` and the caller treats it as a failed source.
//!
//! The helper (`workspace/tools/local-browser/index.mjs`) takes one JSON
//! request on stdin and writes one JSON object per line on stdout.

use crate::security::SecurityPolicy;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

/// URLs accepted per call (the helper enforces the same cap).
pub const MAX_URLS: usize = 20;
/// One browser at a time: a Chrome profile directory cannot be shared by two
/// running instances.
static RUN_SLOT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(1));
const SCRIPT_REL: [&str; 2] = ["local-browser", "index.mjs"];
/// Launch plus profile load; the per-URL budget is added on top.
const BASE_TIMEOUT_SECS: u64 = 90;
const PER_URL_TIMEOUT_SECS: u64 = 60;
/// Settle time after a page's DOM is ready, for script-built content.
const WAIT_MS: u64 = 3500;

/// One fetched page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fetched {
    pub url: String,
    pub final_url: String,
    pub title: String,
    /// Full page HTML after scripts have run.
    pub html: String,
}

/// Per-URL outcome, in request order.
pub type Outcome = std::result::Result<Fetched, String>;

fn script_path(security: &SecurityPolicy) -> Result<PathBuf> {
    let path = security
        .workspace_dir
        .join("tools")
        .join(SCRIPT_REL[0])
        .join(SCRIPT_REL[1]);
    anyhow::ensure!(
        path.exists(),
        "本地浏览器未安装：{} 不存在（需要 node 和 playwright-core）",
        path.display()
    );
    Ok(path)
}

/// Split the helper's JSON lines into per-URL outcomes, in `urls` order.
pub fn parse_output(lines: &str, urls: &[String]) -> Vec<Outcome> {
    let results: Vec<Value> = lines
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(|v| v.get("url").is_some())
        .collect();
    urls.iter()
        .map(|url| {
            let Some(v) = results
                .iter()
                .find(|v| v.get("url").and_then(Value::as_str) == Some(url.as_str()))
            else {
                return Err("本地浏览器没有返回这个网址的结果".to_string());
            };
            let str_of = |k: &str| {
                v.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            if v.get("ok").and_then(Value::as_bool) != Some(true) {
                let code = v.get("error").and_then(Value::as_str).unwrap_or("error");
                let message = str_of("message");
                let status = v.get("status").and_then(Value::as_u64).unwrap_or(0);
                return Err(match (code, status) {
                    ("challenge", _) => format!("本地浏览器: 人机验证页未通过（{message}）"),
                    (c, 0) => format!("本地浏览器: {c} {message}"),
                    (c, s) => format!("本地浏览器: {c}（HTTP {s}）{message}"),
                });
            }
            let html = str_of("html");
            if html.trim().is_empty() {
                return Err("本地浏览器: 页面没有内容".to_string());
            }
            Ok(Fetched {
                url: url.clone(),
                final_url: {
                    let f = str_of("final_url");
                    if f.is_empty() {
                        url.clone()
                    } else {
                        f
                    }
                },
                title: str_of("title"),
                html,
            })
        })
        .collect()
}

/// Fetch up to `MAX_URLS` pages in one browser session.
pub async fn fetch(security: &SecurityPolicy, urls: &[String]) -> Result<Vec<Outcome>> {
    anyhow::ensure!(
        !urls.is_empty() && urls.len() <= MAX_URLS,
        "本地浏览器每次抓取 1 到 {MAX_URLS} 个网址"
    );
    let script = script_path(security)?;
    let dir = script.parent().map(PathBuf::from).unwrap_or_default();
    let _slot = RUN_SLOT
        .acquire()
        .await
        .map_err(|e| anyhow::anyhow!("本地浏览器并发控制异常: {e}"))?;

    let request = json!({"urls": urls, "wait_ms": WAIT_MS}).to_string();
    let mut child = tokio::process::Command::new("node")
        .arg(&script)
        .arg("fetch")
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("无法启动 node（本地浏览器）")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(request.as_bytes()).await.ok();
        stdin.shutdown().await.ok();
    }

    let budget = BASE_TIMEOUT_SECS + PER_URL_TIMEOUT_SECS * urls.len() as u64;
    let output =
        match tokio::time::timeout(Duration::from_secs(budget), child.wait_with_output()).await {
            Ok(result) => result.context("本地浏览器执行失败")?,
            Err(_) => anyhow::bail!("本地浏览器超时（{budget}秒）"),
        };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_output(&stdout, urls);
    if parsed.iter().all(std::result::Result::is_err) && stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "本地浏览器没有输出（退出码 {:?}）: {}",
            output.status.code(),
            crate::util::truncate_with_ellipsis(stderr.trim(), 200)
        );
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(hosts: &[&str]) -> Vec<String> {
        hosts.iter().map(|h| format!("https://{h}/")).collect()
    }

    #[test]
    fn parse_output_keeps_request_order_and_explains_failures() {
        let lines = concat!(
            r#"{"ok":true,"url":"https://b.example.com/","final_url":"https://b.example.com/x","status":200,"title":"B","html":"<html>hi</html>"}"#,
            "\n",
            r#"{"ok":false,"url":"https://a.example.com/","status":403,"error":"challenge","message":"被挡住"}"#,
            "\n",
            r#"{"ok":false,"url":"https://c.example.com/","status":0,"error":"fetch_error","message":"timeout"}"#,
            "\n",
        );
        let out = parse_output(
            lines,
            &urls(&[
                "a.example.com",
                "b.example.com",
                "c.example.com",
                "d.example.com",
            ]),
        );
        assert!(out[0].as_ref().unwrap_err().contains("人机验证页未通过"));
        let b = out[1].as_ref().unwrap();
        assert_eq!(
            (b.title.as_str(), b.final_url.as_str()),
            ("B", "https://b.example.com/x")
        );
        assert!(out[2].as_ref().unwrap_err().contains("fetch_error"));
        assert!(out[3].as_ref().unwrap_err().contains("没有返回"));
    }

    #[test]
    fn empty_html_counts_as_a_failure() {
        let lines = r#"{"ok":true,"url":"https://a.example.com/","status":200,"html":"   "}"#;
        let out = parse_output(lines, &urls(&["a.example.com"]));
        assert!(out[0].as_ref().unwrap_err().contains("没有内容"));
    }

    #[test]
    fn logger_noise_without_a_url_is_ignored() {
        let lines = concat!(
            r#"{"level":30,"msg":"starting"}"#,
            "\n",
            r#"{"ok":true,"url":"https://a.example.com/","status":200,"title":"A","html":"<p>x</p>"}"#,
            "\n",
        );
        let out = parse_output(lines, &urls(&["a.example.com"]));
        assert_eq!(out[0].as_ref().unwrap().html, "<p>x</p>");
    }
}
