//! elfClaw 2026-09-24: the chat agent's only way to manage daily news push
//! slots. Typed actions instead of editing a data file: code validates every
//! change against the HEARTBEAT.md `news-rules`, saves it, and immediately
//! syncs the `news:<slot>` cron jobs (see `crate::cron::news`).

use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::news::{self, Source};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write as _;
use std::sync::Arc;

pub struct NewsScheduleTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl NewsScheduleTool {
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
impl Tool for NewsScheduleTool {
    fn name(&self) -> &str {
        "news_schedule"
    }

    fn description(&self) -> &str {
        "管理每天定时推送的新闻时段和新闻源。action：list（查看全部时段、源、封禁记录、候选源）| \
         set_slot（新建或修改时段：name 是时段的身份，同名即修改；新建需要 time 和 sources）| \
         remove_slot | add_source | remove_source | unban_source（清除某个源的失败/封禁记录）。\
         改动立即生效，系统自动建/改/删对应的定时任务——新闻推送任务不要用 cron_add 建，也不要用 file_write 改数据文件。\
         推送对象、执行的子 agent、静默时段和数量上限由 HEARTBEAT.md 规定，超出会被拒绝。"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "set_slot", "remove_slot", "add_source", "remove_source", "unban_source"]
                },
                "name": { "type": "string", "description": "时段名（set_slot/remove_slot/add_source/remove_source）" },
                "time": { "type": "string", "description": "推送时间 HH:MM（悉尼时间）" },
                "focus": { "type": "string", "description": "本时段关注重点" },
                "sources": {
                    "type": "array",
                    "description": "set_slot 用：整组替换该时段的新闻源",
                    "items": {
                        "type": "object",
                        "properties": {
                            "url": { "type": "string" },
                            "note": { "type": "string" }
                        },
                        "required": ["url"]
                    }
                },
                "url": { "type": "string", "description": "add_source/remove_source/unban_source 用" },
                "note": { "type": "string", "description": "add_source 用：源的备注" }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let text = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let Some(action) = text("action") else {
            return Ok(fail("Missing 'action'"));
        };

        let rules = match news::load_rules(&self.config) {
            Ok(rules) => rules,
            Err(e) => return Ok(fail(format!("{e:#}"))),
        };

        if action == "list" {
            return Ok(match news::load_data(&self.config) {
                Ok(data) => ToolResult {
                    success: true,
                    output: news::summary(&data, &rules),
                    error: None,
                },
                Err(e) => fail(format!("{e:#}")),
            });
        }

        if !self.security.can_act() {
            return Ok(fail(
                "Security policy: read-only mode, cannot change news slots",
            ));
        }
        if self.security.is_rate_limited() || !self.security.record_action() {
            return Ok(fail("Rate limit exceeded"));
        }

        let name = text("name");
        let url = text("url");
        let need = |v: Option<&str>, key: &str| {
            v.map(str::to_string)
                .ok_or_else(|| anyhow::anyhow!("action '{action}' 需要参数 {key}"))
        };
        let sources: Option<Vec<Source>> = match args.get("sources") {
            Some(v) => match serde_json::from_value::<Vec<Source>>(v.clone()) {
                Ok(list) => Some(list),
                Err(e) => return Ok(fail(format!("sources 格式错误: {e}"))),
            },
            None => None,
        };

        let changed = news::update_data(&self.config, &rules, |data| match action {
            "set_slot" => {
                let name = need(name, "name")?;
                let created = news::set_slot(data, &name, text("time"), text("focus"), sources)?;
                Ok(format!(
                    "已{}时段「{name}」",
                    if created { "新建" } else { "修改" }
                ))
            }
            "remove_slot" => {
                let name = need(name, "name")?;
                news::remove_slot(data, &name)?;
                Ok(format!("已删除时段「{name}」"))
            }
            "add_source" => {
                let (name, url) = (need(name, "name")?, need(url, "url")?);
                news::add_source(data, &name, &url, text("note").unwrap_or_default())?;
                Ok(format!("已给时段「{name}」添加 {url}"))
            }
            "remove_source" => {
                let (name, url) = (need(name, "name")?, need(url, "url")?);
                news::remove_source(data, &name, &url)?;
                Ok(format!("已从时段「{name}」删除 {url}"))
            }
            "unban_source" => {
                let url = need(url, "url")?;
                news::unban_source(data, &url)?;
                Ok(format!("已清除 {url} 的失败/封禁记录"))
            }
            other => anyhow::bail!("未知 action '{other}'"),
        });
        let message = match changed {
            Ok(m) => m,
            Err(e) => return Ok(fail(format!("{e:#}"))),
        };

        // The change is saved; sync the cron jobs right away.
        let mut output = message;
        match news::reconcile(&self.config) {
            Ok(report) => {
                for err in &report.errors {
                    let _ = write!(output, "\n⚠️ {err}");
                }
            }
            Err(e) => {
                let _ = write!(output, "\n⚠️ 定时任务同步失败：{e:#}");
            }
        }
        if let Some(name) = name {
            if let Ok(Some(job)) =
                crate::cron::find_job_by_name(&self.config, &news::job_name(name))
            {
                let when = match rules.tz.parse::<chrono_tz::Tz>() {
                    Ok(tz) => job
                        .next_run
                        .with_timezone(&tz)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                    Err(_) => job.next_run.to_rfc3339(),
                };
                let _ = write!(output, "\n下次推送：{when}（{}）", rules.tz);
            }
        }
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
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

    async fn run(tool: &NewsScheduleTool, args: serde_json::Value) -> ToolResult {
        tool.execute(args).await.unwrap()
    }

    #[tokio::test]
    async fn refuses_when_heartbeat_has_no_rules() {
        let (_tmp, config, security) = setup("# no rules");
        let tool = NewsScheduleTool::new(config, security);
        let r = run(&tool, json!({"action": "list"})).await;
        assert!(!r.success);
        assert!(r.error.unwrap_or_default().contains("news-rules"));
    }

    #[tokio::test]
    async fn set_slot_creates_the_job_immediately_and_same_name_updates_it() {
        let (_tmp, config, security) = setup(RULES);
        let tool = NewsScheduleTool::new(config.clone(), security);
        let r = run(
            &tool,
            json!({
                "action": "set_slot", "name": "早报", "time": "06:30", "focus": "国际",
                "sources": [{"url": "https://a.example.com", "note": "A"}]
            }),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        assert!(r.output.contains("下次推送"));
        let job = crate::cron::find_job_by_name(&config, "news:早报")
            .unwrap()
            .unwrap();
        // Code-run news job: the prompt only names the slot.
        assert_eq!(job.job_type, crate::cron::JobType::News);
        assert_eq!(job.prompt.as_deref(), Some("早报"));

        let r = run(
            &tool,
            json!({"action": "set_slot", "name": "早报", "time": "07:15"}),
        )
        .await;
        assert!(r.success, "{:?}", r.error);
        let jobs = crate::cron::list_jobs(&config).unwrap();
        assert_eq!(jobs.len(), 1, "same name must update, not duplicate");
        assert_eq!(
            jobs[0].schedule,
            crate::cron::Schedule::Cron {
                expr: "15 7 * * *".into(),
                tz: Some("Australia/Sydney".into())
            }
        );
    }

    #[tokio::test]
    async fn rule_violations_are_rejected_and_nothing_changes() {
        let (_tmp, config, security) = setup(RULES);
        let tool = NewsScheduleTool::new(config.clone(), security);
        let quiet = run(
            &tool,
            json!({
                "action": "set_slot", "name": "夜报", "time": "23:30",
                "sources": [{"url": "https://a.example.com"}]
            }),
        )
        .await;
        assert!(!quiet.success);
        assert!(quiet.error.unwrap_or_default().contains("静默"));
        let no_sources = run(
            &tool,
            json!({"action": "set_slot", "name": "空", "time": "08:00"}),
        )
        .await;
        assert!(!no_sources.success);
        assert!(crate::cron::list_jobs(&config).unwrap().is_empty());
        assert!(!crate::cron::news::data_path(&config).exists());
    }

    #[tokio::test]
    async fn remove_slot_removes_the_job_and_sources_can_be_edited() {
        let (_tmp, config, security) = setup(RULES);
        let tool = NewsScheduleTool::new(config.clone(), security);
        run(
            &tool,
            json!({
                "action": "set_slot", "name": "科技", "time": "09:30",
                "sources": [{"url": "https://a.example.com"}]
            }),
        )
        .await;
        let added = run(
            &tool,
            json!({"action": "add_source", "name": "科技", "url": "https://b.example.com"}),
        )
        .await;
        assert!(added.success, "{:?}", added.error);
        // Sources are read from the data file at push time.
        let data = news::load_data(&config).unwrap();
        assert!(data.slots[0]
            .sources
            .iter()
            .any(|s| s.url == "https://b.example.com"));

        let removed = run(
            &tool,
            json!({"action": "remove_source", "name": "科技", "url": "https://a.example.com"}),
        )
        .await;
        assert!(removed.success, "{:?}", removed.error);
        let last = run(
            &tool,
            json!({"action": "remove_source", "name": "科技", "url": "https://b.example.com"}),
        )
        .await;
        assert!(!last.success, "cannot remove the last source");

        let gone = run(&tool, json!({"action": "remove_slot", "name": "科技"})).await;
        assert!(gone.success, "{:?}", gone.error);
        assert!(crate::cron::list_jobs(&config).unwrap().is_empty());

        let list = run(&tool, json!({"action": "list"})).await;
        assert!(list.success);
    }
}
