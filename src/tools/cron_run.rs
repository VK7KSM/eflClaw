use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

pub struct CronRunTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronRunTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }
}

#[async_trait]
impl Tool for CronRunTool {
    fn name(&self) -> &str {
        "cron_run"
    }

    fn description(&self) -> &str {
        "Force-run a cron job immediately and record run history"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" }
            },
            "required": ["job_id"]
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

        let job_id = match args.get("job_id").and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'job_id' parameter".to_string()),
                });
            }
        };
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Security policy: read-only mode, cannot perform 'cron_run'".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        let job = match cron::get_job(&self.config, job_id) {
            Ok(job) => job,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let started_at = Utc::now();
        let (ran_ok, output) = Box::pin(cron::scheduler::execute_job_now(&self.config, &job)).await;
        let finished_at = Utc::now();
        let duration_ms = (finished_at - started_at).num_milliseconds();

        // elfClaw 2026-09-23: this used to call record_run/record_last_run
        // directly and stop there — a manually-triggered run never actually
        // delivered its output anywhere, and a one-shot job kept sitting in
        // the list to fire again at its originally scheduled time later.
        // persist_job_result is the same finish-the-lifecycle path the
        // scheduler itself uses: it records the run AND delivers (if
        // configured) AND cleans up a one-shot job. Its returned `success`
        // can differ from `ran_ok` — e.g. the job itself succeeded but
        // non-best-effort delivery failed — so recompute `status` from it
        // rather than the pre-delivery value.
        let success = cron::scheduler::persist_job_result(
            &self.config,
            &job,
            ran_ok,
            &output,
            started_at,
            finished_at,
        )
        .await;
        let status = if success { "ok" } else { "error" };

        Ok(ToolResult {
            success,
            output: serde_json::to_string_pretty(&json!({
                "job_id": job.id,
                "status": status,
                "duration_ms": duration_ms,
                "output": output
            }))?,
            error: if success {
                None
            } else {
                Some("cron job execution failed".to_string())
            },
        })
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
    async fn force_runs_job_and_records_history() {
        // elfClaw 2026-09-23: uses a message job (no LLM call, always
        // succeeds deterministically) rather than an agent job — this test
        // is about the run-history recording mechanism, not about actually
        // executing a job, and agent jobs need a real provider to succeed.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_message_job(
            &cfg,
            None,
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "run-now reminder",
            None,
            false,
        )
        .unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        let runs = cron::list_runs(&cfg, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[tokio::test]
    async fn manually_running_a_one_shot_job_deletes_it() {
        // elfClaw 2026-09-23: this used to record the run and stop — the
        // one-shot job stayed in the list and would still fire again later
        // at its originally scheduled time.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = crate::cron::add_message_job(
            &cfg,
            Some("manual-once".into()),
            crate::cron::Schedule::At {
                at: Utc::now() + chrono::Duration::minutes(10),
            },
            "reminder text",
            None,
            true,
        )
        .unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(result.success, "{:?}", result.error);

        assert!(
            cron::get_job(&cfg, &job.id).is_err(),
            "manually running a one-shot job should delete it, not leave it to fire again"
        );
    }

    #[tokio::test]
    async fn message_job_runs_with_no_delivery_configured_and_succeeds() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = crate::cron::add_message_job(
            &cfg,
            None,
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "hello",
            None, // no delivery configured — should still "succeed", just deliver nowhere
            false,
        )
        .unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn errors_for_missing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({ "job_id": "missing-job-id" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("not found"));
    }

    #[tokio::test]
    async fn blocks_run_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    #[tokio::test]
    async fn blocks_run_when_rate_limited() {
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
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Rate limit exceeded"));
        assert!(cron::list_runs(&cfg, &job.id, 10).unwrap().is_empty());
    }
}
