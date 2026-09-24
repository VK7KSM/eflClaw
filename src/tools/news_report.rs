//! elfClaw 2026-09-24: how the news worker reports back. Failure counting,
//! banning and candidate de-duplication are done by code
//! (`crate::cron::news`), not by the model editing a text list.

use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::news::{self, Candidate, SourceResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;

pub struct NewsReportTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl NewsReportTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }
}

fn fail(msg: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(msg.into()),
    }
}

#[async_trait]
impl Tool for NewsReportTool {
    fn name(&self) -> &str {
        "news_report"
    }

    fn description(&self) -> &str {
        "新闻采集用。action=report：抓完本时段所有源后调用一次，results 里每个源一条 {url, ok, reason}，\
         系统负责计数，同一个源失败达到上限会被自动封禁、以后不再出现在任务里，抓成功会清掉失败记录。\
         action=add_candidates：登记新发现的候选新闻源 {name, category, kind, url, note}，重复的会被自动忽略。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["report", "add_candidates"] },
                "results": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string" },
                            "ok": { "type": "boolean" },
                            "reason": { "type": "string", "description": "失败原因，如 403 / 超时" }
                        },
                        "required": ["url", "ok"]
                    }
                },
                "candidates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "category": { "type": "string" },
                            "kind": { "type": "string", "description": "RSS 或 网页" },
                            "url": { "type": "string" },
                            "note": { "type": "string" }
                        },
                        "required": ["name", "url"]
                    }
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let action = args
            .get("action")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !self.security.can_act() {
            return Ok(fail("Security policy: read-only mode"));
        }
        let rules = match news::load_rules(&self.config) {
            Ok(rules) => rules,
            Err(e) => return Ok(fail(format!("{e:#}"))),
        };

        match action {
            "report" => {
                let results: Vec<SourceResult> = match args.get("results") {
                    Some(v) => match serde_json::from_value(v.clone()) {
                        Ok(r) => r,
                        Err(e) => return Ok(fail(format!("results 格式错误: {e}"))),
                    },
                    None => return Ok(fail("report 需要 results")),
                };
                let now = chrono::Utc::now().to_rfc3339();
                let outcome = match news::update_data(&self.config, &rules, |data| {
                    Ok(news::record_results(data, &rules, &results, &now))
                }) {
                    Ok(o) => o,
                    Err(e) => return Ok(fail(format!("{e:#}"))),
                };
                // A newly banned source must drop out of the slot's task text.
                if !outcome.newly_banned.is_empty() {
                    if let Err(e) = news::reconcile(&self.config) {
                        tracing::warn!("news reconcile after ban failed: {e:#}");
                    }
                }
                let mut output = format!("已记录 {} 个源的结果。", outcome.recorded);
                if !outcome.newly_banned.is_empty() {
                    let _ = write!(
                        output,
                        "\n新封禁（以后不再抓）：{}",
                        outcome.newly_banned.join(", ")
                    );
                }
                if !outcome.watching.is_empty() {
                    let _ = write!(output, "\n观察中：{}", outcome.watching.join(", "));
                }
                if !outcome.ignored.is_empty() {
                    let _ = write!(
                        output,
                        "\n忽略（不是任何时段的源）：{}",
                        outcome.ignored.join(", ")
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            "add_candidates" => {
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let mut items: Vec<Candidate> = match args.get("candidates") {
                    Some(v) => match serde_json::from_value::<Vec<serde_json::Value>>(v.clone()) {
                        Ok(list) => {
                            let mut out = Vec::new();
                            for mut item in list {
                                if item.get("found").is_none() {
                                    item["found"] = json!(today);
                                }
                                match serde_json::from_value::<Candidate>(item) {
                                    Ok(c) => out.push(c),
                                    Err(e) => return Ok(fail(format!("candidates 格式错误: {e}"))),
                                }
                            }
                            out
                        }
                        Err(e) => return Ok(fail(format!("candidates 格式错误: {e}"))),
                    },
                    None => return Ok(fail("add_candidates 需要 candidates")),
                };
                items.retain(|c| !c.url.trim().is_empty());
                match news::update_data(&self.config, &rules, |data| {
                    news::add_candidates(data, items)
                }) {
                    Ok((added, skipped)) => Ok(ToolResult {
                        success: true,
                        output: format!("新增候选源 {added} 个，跳过重复 {skipped} 个"),
                        error: None,
                    }),
                    Err(e) => Ok(fail(format!("{e:#}"))),
                }
            }
            other => Ok(fail(format!("未知 action '{other}'"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const RULES: &str = r#"<!-- news-rules
agent = "news_fetcher"
delivery = { mode = "announce", channel = "telegram", to = "zeroclaw_user" }
tz = "Australia/Sydney"
quiet_start = "23:00"
quiet_end = "06:30"
max_slots = 3
max_sources_per_slot = 3
ban_after_failures = 3
-->"#;

    fn setup(heartbeat: &str) -> (TempDir, Arc<Config>, Arc<SecurityPolicy>) {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        std::fs::write(config.workspace_dir.join("HEARTBEAT.md"), heartbeat).unwrap();
        let agent: crate::config::DelegateAgentConfig =
            toml::from_str(r#"allowed_tools = ["news_report"]"#).unwrap();
        config.agents.insert("news_fetcher".into(), agent);
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        (tmp, Arc::new(config), security)
    }

    #[tokio::test]
    async fn third_failure_bans_the_source_and_drops_it_from_the_task() {
        let (_tmp, config, security) = setup(RULES);
        let rules = news::load_rules(&config).unwrap();
        news::update_data(&config, &rules, |d| {
            news::set_slot(
                d,
                "科技",
                Some("09:30"),
                None,
                Some(vec![
                    news::Source {
                        url: "https://a.example.com".into(),
                        note: String::new(),
                    },
                    news::Source {
                        url: "https://b.example.com".into(),
                        note: String::new(),
                    },
                ]),
            )
        })
        .unwrap();
        news::reconcile(&config).unwrap();

        let tool = NewsReportTool::new(config.clone(), security);
        let report = json!({"action": "report", "results": [
            {"url": "https://a.example.com", "ok": false, "reason": "403"},
            {"url": "https://b.example.com", "ok": true}
        ]});
        for _ in 0..2 {
            let r = tool.execute(report.clone()).await.unwrap();
            assert!(r.success, "{:?}", r.error);
            assert!(r.output.contains("观察中"));
        }
        let r = tool.execute(report).await.unwrap();
        assert!(r.output.contains("新封禁"));
        let prompt = crate::cron::find_job_by_name(&config, "news:科技")
            .unwrap()
            .unwrap()
            .prompt
            .unwrap();
        assert!(!prompt.contains("https://a.example.com"));
        assert!(prompt.contains("https://b.example.com"));
    }

    #[tokio::test]
    async fn add_candidates_dedups_and_stamps_the_date() {
        let (_tmp, config, security) = setup(RULES);
        let tool = NewsReportTool::new(config.clone(), security);
        let r = tool
            .execute(json!({"action": "add_candidates", "candidates": [
                {"name": "A", "url": "https://a.example.com"},
                {"name": "A again", "url": "https://a.example.com"}
            ]}))
            .await
            .unwrap();
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.contains("新增候选源 1 个"));
        let data = news::load_data(&config).unwrap();
        assert_eq!(data.candidates.len(), 1);
        assert!(!data.candidates[0].found.is_empty());
    }
}
