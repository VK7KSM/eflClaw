//! elfClaw 2026-09-25: TinyFish page fetch through the Monid gateway.
//!
//! TinyFish runs its own stealth Chromium fleet and gets through bot checks
//! that stop both plain HTTP and Cloudflare Browser Rendering (tested:
//! scarletblue.com.au, fuzoku.jp). The fetch endpoint is free on Monid.
//! Used by the expo / adult pipelines for sources marked `tinyfish = true`.
//!
//! The Monid API key is read from the `MONID_API_KEY` environment variable,
//! like cf-crawler's `CF_CRAWLER_TOKEN`; it is only ever sent to the fixed
//! Monid host and never logged.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const RUN_URL: &str = "https://api.monid.ai/v1/run";
const RUNS_URL: &str = "https://api.monid.ai/v1/runs";
pub const KEY_ENV: &str = "MONID_API_KEY";
/// TinyFish accepts at most this many URLs per request.
pub const MAX_URLS: usize = 10;
const PER_URL_TIMEOUT_MS: u64 = 60_000;
const REQUEST_TIMEOUT_SECS: u64 = 150;
/// Async runs (HTTP 202) are polled this many times, `POLL_SECS` apart.
const POLL_ATTEMPTS: usize = 30;
const POLL_SECS: u64 = 4;

/// One fetched page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Fetched {
    pub url: String,
    /// Page content as Markdown.
    pub text: String,
    /// Absolute URLs of every link on the page.
    pub links: Vec<String>,
}

/// Per-URL outcome, in request order.
pub type Outcome = std::result::Result<Fetched, String>;

fn key() -> Result<String> {
    let key = std::env::var(KEY_ENV).unwrap_or_default();
    if key.trim().is_empty() {
        bail!("没有设置环境变量 {KEY_ENV}（Monid API key），无法用 TinyFish 抓取");
    }
    Ok(key.trim().to_string())
}

/// Split a finished run's `output` into per-URL outcomes, in `urls` order.
pub fn parse_output(output: &Value, urls: &[String]) -> Vec<Outcome> {
    let results = output
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let errors = output
        .get("errors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    urls.iter()
        .map(|url| {
            let same = |v: &Value| v.get("url").and_then(Value::as_str) == Some(url.as_str());
            if let Some(r) = results.iter().find(|r| same(r)) {
                return Ok(Fetched {
                    url: url.clone(),
                    text: r
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    links: r
                        .get("links")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|l| l.as_str().map(str::to_string))
                        .collect(),
                });
            }
            match errors.iter().find(|e| same(e)) {
                Some(e) => {
                    let code = e.get("error").and_then(Value::as_str).unwrap_or("error");
                    Err(match e.get("status").and_then(Value::as_u64) {
                        Some(status) => format!("TinyFish: {code}（HTTP {status}）"),
                        None => format!("TinyFish: {code}"),
                    })
                }
                None => Err("TinyFish 没有返回这个网址的结果".to_string()),
            }
        })
        .collect()
}

/// Fetch up to `MAX_URLS` pages in one TinyFish request.
pub async fn fetch(client: &reqwest::Client, urls: &[String]) -> Result<Vec<Outcome>> {
    anyhow::ensure!(
        !urls.is_empty() && urls.len() <= MAX_URLS,
        "TinyFish 每次抓取 1 到 {MAX_URLS} 个网址"
    );
    let key = key()?;
    let body = json!({
        "provider": "tinyfish",
        "endpoint": "/fetch",
        "input": {"body": {
            "urls": urls,
            "format": "markdown",
            "links": true,
            "per_url_timeout_ms": PER_URL_TIMEOUT_MS,
        }},
    });
    let resp = client
        .post(RUN_URL)
        .bearer_auth(&key)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
        .context("请求 Monid 失败")?;
    let status = resp.status();
    let mut run: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let message = run
            .pointer("/error/message")
            .or_else(|| run.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        bail!("Monid 返回 HTTP {}：{message}", status.as_u16());
    }
    for _ in 0..POLL_ATTEMPTS {
        match run.get("status").and_then(Value::as_str) {
            Some("COMPLETED") => return Ok(parse_output(&run["output"], urls)),
            Some(s @ ("FAILED" | "BLOCKED" | "STOPPED" | "TIMED_OUT")) => {
                bail!("TinyFish 抓取没有完成（{s}）")
            }
            _ => {}
        }
        let Some(id) = run.get("runId").and_then(Value::as_str).map(str::to_string) else {
            bail!("Monid 的返回里没有运行编号");
        };
        tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
        run = client
            .get(format!("{RUNS_URL}/{id}"))
            .bearer_auth(&key)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("查询 Monid 运行状态失败")?
            .json()
            .await
            .unwrap_or(Value::Null);
    }
    bail!(
        "TinyFish 抓取超时（等了 {} 秒）",
        POLL_ATTEMPTS as u64 * POLL_SECS
    )
}

/// Link text for a bare URL: the last path segment with separators as spaces
/// (`/escort/lucia-valmont` → `lucia valmont`), so name matching still works.
pub fn slug_text(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(['-', '_', '.', '?'])
        .filter(|w| !w.is_empty() && *w != "html" && *w != "php")
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_keeps_request_order_and_reports_errors() {
        let output = json!({
            "results": [{"url": "https://b.example.com/", "text": "# B", "links": ["https://b.example.com/x"]}],
            "errors": [{"url": "https://a.example.com/", "error": "bot_blocked"},
                       {"url": "https://c.example.com/", "error": "target_http_error", "status": 403}]
        });
        let urls: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|h| format!("https://{h}.example.com/"))
            .collect();
        let out = parse_output(&output, &urls);
        assert_eq!(out[0], Err("TinyFish: bot_blocked".to_string()));
        let b = out[1].as_ref().unwrap();
        assert_eq!((b.text.as_str(), b.links.len()), ("# B", 1));
        assert_eq!(
            out[2],
            Err("TinyFish: target_http_error（HTTP 403）".to_string())
        );
        assert!(out[3].is_err());
    }

    #[test]
    fn slug_text_turns_profile_urls_into_words() {
        assert_eq!(
            slug_text("https://scarletblue.com.au/escort/lucia-valmont"),
            "lucia valmont"
        );
        assert_eq!(
            slug_text("https://x.example.com/a/big_tina.html/"),
            "big tina"
        );
    }
}
