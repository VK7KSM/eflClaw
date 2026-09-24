use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronRemoveTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronRemoveTool {
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
impl Tool for CronRemoveTool {
    fn name(&self) -> &str {
        "cron_remove"
    }

    fn description(&self) -> &str {
        "Remove cron jobs. Pass 'name' to remove every job with that name (preferred — \
removes duplicates too), or 'job_id' to remove one specific job. Jobs whose names start \
with heartbeat:/news:/note: are managed elsewhere and cannot be removed here."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Remove every job with exactly this name."
                },
                "job_id": {
                    "type": "string",
                    "description": "Remove one job by id."
                }
            }
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

        let arg = |key: &str| {
            args.get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let (name, job_id) = (arg("name"), arg("job_id"));

        // Resolve the name of what is about to be removed, so managed jobs
        // are refused before anything is touched.
        let target_name = match (name, job_id) {
            (Some(n), _) => Some(n.to_string()),
            (None, Some(id)) => match cron::get_job(&self.config, id) {
                Ok(job) => job.name,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(e.to_string()),
                    });
                }
            },
            (None, None) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Provide 'name' (preferred) or 'job_id'".to_string()),
                });
            }
        };
        if let Some(owner) = target_name.as_deref().and_then(cron::managed_by) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "'{}' is managed by {owner}; removing it here would just be recreated. Change it through {owner} instead.",
                    target_name.unwrap_or_default()
                )),
            });
        }

        if let Some(blocked) = self.enforce_mutation_allowed("cron_remove") {
            return Ok(blocked);
        }

        if let Some(name) = name {
            return Ok(match cron::remove_jobs_by_name(&self.config, name) {
                Ok(0) => ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("No cron job named '{name}'")),
                },
                Ok(n) => ToolResult {
                    success: true,
                    output: format!("Removed {n} cron job(s) named '{name}'"),
                    error: None,
                },
                Err(e) => ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                },
            });
        }
        let job_id = job_id.unwrap_or_default();

        match cron::remove_job(&self.config, job_id) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Removed cron job {job_id}"),
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
    async fn removes_existing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(result.success);
        assert!(cron::list_jobs(&cfg).unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_when_neither_name_nor_job_id_given() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("Provide 'name'"));
    }

    #[tokio::test]
    async fn blocks_remove_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    #[tokio::test]
    async fn blocks_remove_when_rate_limited() {
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
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Rate limit exceeded"));
        assert_eq!(cron::list_jobs(&cfg).unwrap().len(), 1);
    }

    // ── elfClaw 2026-09-24 ──

    fn msg_job(cfg: &Config, name: &str, expr: &str) -> crate::cron::CronJob {
        crate::cron::add_message_job(
            cfg,
            Some(name.into()),
            crate::cron::Schedule::Cron {
                expr: expr.into(),
                tz: None,
            },
            "hi",
            None,
            false,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn remove_by_name_removes_every_duplicate() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        msg_job(&cfg, "吃药提醒", "0 8 * * *");
        let second = msg_job(&cfg, "临时", "0 9 * * *");
        // Force a legacy same-name duplicate (what older builds left behind).
        crate::cron::update_job(
            &cfg,
            &second.id,
            crate::cron::CronJobPatch {
                name: Some("吃药提醒".into()),
                ..crate::cron::CronJobPatch::default()
            },
        )
        .unwrap();
        assert_eq!(cron::list_jobs(&cfg).unwrap().len(), 2);

        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));
        let result = tool.execute(json!({"name": "吃药提醒"})).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("Removed 2"));
        assert!(cron::list_jobs(&cfg).unwrap().is_empty());
    }

    #[tokio::test]
    async fn refuses_to_remove_managed_jobs_by_name_or_id() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = msg_job(&cfg, "heartbeat:早报综合", "30 6 * * *");
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let by_name = tool
            .execute(json!({"name": "heartbeat:早报综合"}))
            .await
            .unwrap();
        assert!(!by_name.success);
        assert!(by_name.error.unwrap_or_default().contains("HEARTBEAT.md"));
        let by_id = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(!by_id.success);
        assert_eq!(cron::list_jobs(&cfg).unwrap().len(), 1);
    }
}
