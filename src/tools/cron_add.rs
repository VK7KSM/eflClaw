use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::{self, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronAddTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

const MIN_AGENT_EVERY_MS: u64 = 5 * 60 * 1000;

impl CronAddTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn enforce_mutation_allowed(&self, action: &str) -> Option<ToolResult> {
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Security policy: read-only mode, cannot perform '{action}'"
                )),
            });
        }

        if self.security.is_rate_limited() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".to_string()),
            });
        }

        if !self.security.record_action() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".to_string()),
            });
        }

        None
    }
}

#[async_trait]
impl Tool for CronAddTool {
    fn name(&self) -> &str {
        "cron_add"
    }

    fn description(&self) -> &str {
        "Create a scheduled cron job (agent or message) with cron/at/every schedules. \
         Use job_type='message' with a fixed 'message' text for a plain reminder/scheduled \
         send — it is delivered exactly as written with NO LLM call at fire time, so prefer \
         it whenever the reminder doesn't need the model to look anything up or decide what \
         to say. Use job_type='agent' with a 'prompt' only when the model actually needs to \
         do something (e.g. fetch and summarize something) at fire time. \
         Use schedule.kind='at' for one-time reminders/delayed sends (recommended). \
         Agent and message jobs with schedule.kind='cron' or schedule.kind='every' are recurring \
         and require explicit recurring confirmation. \
         To deliver output to a channel (Discord, Telegram, Slack, Mattermost, QQ, Napcat, Lark, Feishu, Email), set \
         delivery={\"mode\":\"announce\",\"channel\":\"discord\",\"to\":\"<channel_id_or_chat_id>\"} \
         (required for job_type='message'). \
         This is the preferred tool for sending scheduled/delayed messages to users via channels."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "schedule": {
                    "type": "object",
                    "description": "Schedule object: {kind:'cron',expr,tz?} recurring | {kind:'at',at} one-time | {kind:'every',every_ms} recurring interval"
                },
                "job_type": { "type": "string", "enum": ["agent", "message"] },
                "prompt": { "type": "string" },
                "message": {
                    "type": "string",
                    "description": "For job_type='message': the exact text to deliver at fire time. No model call — sent as-is."
                },
                "session_target": { "type": "string", "enum": ["isolated", "main"] },
                // elfClaw: removed model field — cron jobs must always use worker_model from config at runtime
                "recurring_confirmed": {
                    "type": "boolean",
                    "description": "Required for agent recurring schedules (schedule.kind='cron' or 'every'). Set true only when recurring behavior is intentional.",
                    "default": false
                },
                "delivery": {
                    "type": "object",
                    "description": "Delivery config to send job output to a channel. Example: {\"mode\":\"announce\",\"channel\":\"discord\",\"to\":\"<channel_id>\"}",
                    "properties": {
                        "mode": { "type": "string", "enum": ["none", "announce"], "description": "Set to 'announce' to deliver output to a channel" },
                        "channel": { "type": "string", "enum": ["telegram", "discord", "slack", "mattermost", "qq", "napcat", "lark", "feishu", "email"], "description": "Channel type to deliver to" },
                        "to": { "type": "string", "description": "Target: Discord channel ID, Telegram chat ID, Slack channel, etc." },
                        "best_effort": { "type": "boolean", "description": "If true, delivery failure does not fail the job" }
                    }
                },
                "delete_after_run": { "type": "boolean" },
                "delegate_to": {
                    "type": ["string", "null"],
                    "description": "Name of a configured sub-agent (from [agents.*]) to delegate this job to. When set, the job's prompt is automatically routed to that agent via the delegate tool instead of being executed by the main agent."
                }
            },
            "required": ["schedule"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.config.cron.enabled {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            });
        }

        let schedule = match args.get("schedule") {
            Some(v) => match serde_json::from_value::<Schedule>(v.clone()) {
                Ok(schedule) => schedule,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Invalid schedule: {e}")),
                    });
                }
            },
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'schedule' parameter".to_string()),
                });
            }
        };

        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let job_type = match args.get("job_type").and_then(serde_json::Value::as_str) {
            Some("agent") => JobType::Agent,
            Some("message") => JobType::Message,
            Some(other) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid job_type: {other}")),
                });
            }
            None => {
                if args.get("message").is_some() {
                    JobType::Message
                } else if args.get("prompt").is_some() {
                    JobType::Agent
                } else {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(
                            "Missing 'job_type' (agent|message), or provide 'message' or 'prompt' \
                             so it can be inferred."
                                .to_string(),
                        ),
                    });
                }
            }
        };

        let default_delete_after_run = matches!(schedule, Schedule::At { .. });
        let delete_after_run = args
            .get("delete_after_run")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default_delete_after_run);

        let result = match job_type {
            JobType::Agent => {
                let prompt = match args.get("prompt").and_then(serde_json::Value::as_str) {
                    Some(prompt) if !prompt.trim().is_empty() => prompt,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'prompt' for agent job".to_string()),
                        });
                    }
                };

                let session_target = match args.get("session_target") {
                    Some(v) => match serde_json::from_value::<SessionTarget>(v.clone()) {
                        Ok(target) => target,
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid session_target: {e}")),
                            });
                        }
                    },
                    None => SessionTarget::Isolated,
                };

                // elfClaw: always None — model is resolved at runtime from config (worker_model)
                let model: Option<String> = None;
                let recurring_confirmed = args
                    .get("recurring_confirmed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);

                match &schedule {
                    Schedule::Every { every_ms } => {
                        if !recurring_confirmed {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(
                                    "Agent jobs with recurring schedules require recurring_confirmed=true. \
For one-time reminders, use schedule.kind='at' with an RFC3339 timestamp."
                                        .to_string(),
                                ),
                            });
                        }
                        if *every_ms < MIN_AGENT_EVERY_MS {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!(
                                    "Agent schedule.kind='every' must be >= {MIN_AGENT_EVERY_MS} ms (5 minutes)"
                                )),
                            });
                        }
                    }
                    Schedule::Cron { .. } => {
                        if !recurring_confirmed {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(
                                    "Agent jobs with recurring schedules require recurring_confirmed=true. \
For one-time reminders, use schedule.kind='at' with an RFC3339 timestamp."
                                        .to_string(),
                                ),
                            });
                        }
                    }
                    Schedule::At { .. } => {}
                }

                let delivery = match args.get("delivery") {
                    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid delivery config: {e}")),
                            });
                        }
                    },
                    None => None,
                };

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                let delegate_to = args
                    .get("delegate_to")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);

                cron::add_agent_job(
                    &self.config,
                    name,
                    schedule,
                    prompt,
                    session_target,
                    model,
                    delivery,
                    delete_after_run,
                    delegate_to,
                )
            }
            JobType::Message => {
                let message = match args.get("message").and_then(serde_json::Value::as_str) {
                    Some(message) if !message.trim().is_empty() => message,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'message' for message job".to_string()),
                        });
                    }
                };

                let recurring_confirmed = args
                    .get("recurring_confirmed")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                match &schedule {
                    Schedule::Every { every_ms } if !recurring_confirmed => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Message jobs with recurring schedules require recurring_confirmed=true. \
For a one-time reminder, use schedule.kind='at' with an RFC3339 timestamp."
                                    .to_string(),
                            ),
                        });
                    }
                    Schedule::Cron { .. } if !recurring_confirmed => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "Message jobs with recurring schedules require recurring_confirmed=true. \
For a one-time reminder, use schedule.kind='at' with an RFC3339 timestamp."
                                    .to_string(),
                            ),
                        });
                    }
                    _ => {}
                }

                let delivery = match args.get("delivery") {
                    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid delivery config: {e}")),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "'delivery' (mode='announce', channel, to) is required for \
job_type='message' — a reminder with nowhere to go isn't useful. Use job_type='agent' \
instead if you actually need the model to do something at fire time."
                                    .to_string(),
                            ),
                        });
                    }
                };

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                cron::add_message_job(
                    &self.config,
                    name,
                    schedule,
                    message,
                    Some(delivery),
                    delete_after_run,
                )
            }
        };

        match result {
            Ok(job) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "id": job.id,
                    "name": job.name,
                    "job_type": job.job_type,
                    "schedule": job.schedule,
                    "next_run": job.next_run,
                    "enabled": job.enabled
                }))?,
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> Arc<Config> {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        Arc::new(config)
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::from_config(
            &cfg.autonomy,
            &cfg.workspace_dir,
        ))
    }

    #[tokio::test]
    async fn adds_message_job_with_delivery() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({
                "name": "reminder",
                "schedule": { "kind": "at", "at": (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339() },
                "job_type": "message",
                "message": "带孩子看牙医",
                "delivery": { "mode": "announce", "channel": "telegram", "to": "123" }
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        let jobs = cron::list_jobs(&cfg).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].job_type, JobType::Message);
        assert_eq!(jobs[0].prompt.as_deref(), Some("带孩子看牙医"));
    }

    #[tokio::test]
    async fn message_job_without_delivery_is_rejected() {
        // A reminder with no delivery target would silently go nowhere at
        // fire time — reject it up front instead of letting the user think
        // it was scheduled.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({
                "schedule": { "kind": "at", "at": (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339() },
                "job_type": "message",
                "message": "no delivery"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("delivery"));
    }

    #[tokio::test]
    async fn message_job_requires_message_text() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({
                "schedule": { "kind": "at", "at": (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339() },
                "job_type": "message",
                "delivery": { "mode": "announce", "channel": "telegram", "to": "123" }
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("message"));
    }

    #[tokio::test]
    async fn recurring_message_job_requires_confirmation() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                "job_type": "message",
                "message": "daily reminder",
                "delivery": { "mode": "announce", "channel": "telegram", "to": "123" }
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("recurring_confirmed"));
    }

    #[tokio::test]
    async fn blocks_mutation_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "at", "at": "2099-01-01T00:00:00Z" },
                "job_type": "message",
                "message": "reminder",
                "delivery": {"mode": "announce", "channel": "telegram", "to": "123"}
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("read-only") || error.contains("not allowed"));
    }

    #[tokio::test]
    async fn blocks_add_when_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::Full;
        config.autonomy.max_actions_per_hour = 0;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent",
                "prompt": "do the thing",
                "recurring_confirmed": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Rate limit exceeded"));
        assert!(cron::list_jobs(&cfg).unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_invalid_schedule() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 0 },
                "job_type": "message",
                "message": "reminder",
                "delivery": {"mode": "announce", "channel": "telegram", "to": "123"},
                "recurring_confirmed": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("every_ms must be > 0"));
    }

    #[tokio::test]
    async fn agent_job_requires_prompt() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Missing 'prompt'"));
    }

    #[tokio::test]
    async fn agent_job_with_existing_name_updates_instead_of_no_op() {
        // elfClaw 2026-09-23: pins the actual production bug. This tool used
        // to have its own name-lookup that returned success/"already_exists"
        // with NO changes applied, before the call ever reached
        // `cron::add_agent_job` — which already had correct update-by-name
        // logic that this early return made unreachable. A model correcting
        // a reminder's time (or the heartbeat re-syncing HEARTBEAT.md) would
        // get told "already exists, no action needed" and the job would
        // silently keep its old, wrong schedule/prompt forever.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let first = tool
            .execute(json!({
                "name": "morning-news",
                "schedule": { "kind": "cron", "expr": "30 6 * * *" },
                "job_type": "agent",
                "prompt": "Send the morning digest",
                "recurring_confirmed": true
            }))
            .await
            .unwrap();
        assert!(first.success, "{:?}", first.error);

        let second = tool
            .execute(json!({
                "name": "morning-news",
                "schedule": { "kind": "cron", "expr": "0 7 * * *" },
                "job_type": "agent",
                "prompt": "Send the updated morning digest",
                "recurring_confirmed": true
            }))
            .await
            .unwrap();
        assert!(second.success, "{:?}", second.error);
        assert!(
            !second.output.contains("already_exists"),
            "same-name create should update the job, not report a no-op: {}",
            second.output
        );

        let jobs = cron::list_jobs(&cfg).unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "the second call must update the existing job, not create a duplicate"
        );
        assert_eq!(
            jobs[0].expression, "0 7 * * *",
            "schedule must reflect the update"
        );
        assert_eq!(
            jobs[0].prompt.as_deref(),
            Some("Send the updated morning digest"),
            "prompt must reflect the update"
        );
    }

    #[tokio::test]
    async fn agent_every_requires_recurring_confirmation() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 300000 },
                "job_type": "agent",
                "prompt": "Send me a recurring status update"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("recurring_confirmed=true"));
    }

    #[tokio::test]
    async fn agent_cron_requires_recurring_confirmation() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent",
                "prompt": "Send recurring reminders"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("recurring_confirmed=true"));
    }

    #[tokio::test]
    async fn agent_every_rejects_high_frequency_intervals() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 60000 },
                "job_type": "agent",
                "prompt": "Send me updates frequently",
                "recurring_confirmed": true
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("must be >= 300000 ms"));
    }

    #[tokio::test]
    async fn agent_every_with_explicit_confirmation_succeeds() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 300000 },
                "job_type": "agent",
                "prompt": "Share a heartbeat summary",
                "recurring_confirmed": true
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("next_run"));
    }
}
