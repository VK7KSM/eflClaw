use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::{self, CronJob};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

pub struct CronListTool {
    config: Arc<Config>,
}

/// elfClaw 2026-09-23: `last_output` can be up to 16KB per job (see
/// `cron::store::truncate_cron_output`) and `prompt` can run 1-1.5KB for a
/// realistic agent job — with even a handful of jobs, the old
/// `serde_json::to_string_pretty(&jobs)` output regularly blew past the
/// agent loop's `MAX_TOOL_RESULT_IN_HISTORY_CHARS` (8,000) truncation limit.
/// That's how the heartbeat used to lose track of which jobs already
/// existed: it would see the first one or two jobs followed by "...", and
/// (mis)conclude everything past that point needed to be created again. The
/// heartbeat no longer calls this tool at all (see
/// `cron::heartbeat_decl::reconcile`), but the chat agent still can — this
/// keeps that path bounded too, regardless of job count or verbosity.
const PREVIEW_CHARS: usize = 200;

#[derive(Serialize)]
struct CronJobListEntry<'a> {
    id: &'a str,
    name: Option<&'a str>,
    job_type: &'a crate::cron::JobType,
    schedule: &'a crate::cron::Schedule,
    enabled: bool,
    next_run: chrono::DateTime<chrono::Utc>,
    last_run: Option<chrono::DateTime<chrono::Utc>>,
    last_status: Option<&'a str>,
    /// Truncated preview — not the full prompt/output. There is currently no
    /// tool to fetch one job's untruncated text; `cron_runs` gives full run
    /// history output per run, which is usually what "why did this fail"
    /// questions actually need.
    prompt_preview: Option<String>,
    last_output_preview: Option<String>,
}

fn preview(s: &str) -> String {
    if s.chars().count() <= PREVIEW_CHARS {
        return s.to_string();
    }
    let truncated: String = s.chars().take(PREVIEW_CHARS).collect();
    let total = s.chars().count();
    format!("{truncated}… ({total} chars total, truncated)")
}

fn to_list_entry(job: &CronJob) -> CronJobListEntry<'_> {
    CronJobListEntry {
        id: &job.id,
        name: job.name.as_deref(),
        job_type: &job.job_type,
        schedule: &job.schedule,
        enabled: job.enabled,
        next_run: job.next_run,
        last_run: job.last_run,
        last_status: job.last_status.as_deref(),
        prompt_preview: job.prompt.as_deref().map(preview),
        last_output_preview: job.last_output.as_deref().map(preview),
    }
}

impl CronListTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "cron_list"
    }

    fn description(&self) -> &str {
        "List all scheduled cron jobs. Prompt and last_output are truncated \
         previews (200 chars) to keep the result bounded regardless of job \
         count — use cron_runs for a job's full run output."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.config.cron.enabled {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            });
        }

        match cron::list_jobs(&self.config) {
            Ok(jobs) => {
                let entries: Vec<CronJobListEntry<'_>> = jobs.iter().map(to_list_entry).collect();
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&entries)?,
                    error: None,
                })
            }
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

    #[test]
    fn preview_passes_short_strings_through_unchanged() {
        assert_eq!(preview("short"), "short");
    }

    #[test]
    fn preview_truncates_long_strings_with_char_count() {
        let long = "a".repeat(500);
        let result = preview(&long);
        assert!(result.starts_with(&"a".repeat(PREVIEW_CHARS)));
        assert!(result.contains("500 chars total, truncated"));
        assert!(result.chars().count() < long.chars().count());
    }

    #[test]
    fn preview_truncates_by_char_not_byte_boundary() {
        // Each "中" is 3 bytes — truncating by byte count instead of char
        // count would panic (or split a character) partway through 200
        // repetitions of it. This just needs to not panic and to actually
        // shorten the string.
        let long = "中".repeat(500);
        let result = preview(&long);
        assert!(result.chars().count() < long.chars().count());
    }

    #[tokio::test]
    async fn returns_empty_list_when_no_jobs() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronListTool::new(cfg);

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output.trim(), "[]");
    }

    #[tokio::test]
    async fn errors_when_cron_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = (*test_config(&tmp).await).clone();
        cfg.cron.enabled = false;
        let tool = CronListTool::new(Arc::new(cfg));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("cron is disabled"));
    }

    #[tokio::test]
    async fn output_stays_bounded_for_a_job_with_a_large_prompt() {
        // elfClaw 2026-09-23: this is the actual production shape — a
        // realistic-sized agent prompt across several jobs used to be enough
        // to blow the 8,000-char tool-result history limit and confuse the
        // (now removed) heartbeat cron-sync prompt. One long job here is
        // enough to prove the preview cap is actually wired through the
        // tool's output, not just the standalone `preview()` helper.
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let long_prompt = "x".repeat(5_000);
        crate::cron::add_agent_job(
            &cfg,
            Some("big-job".into()),
            crate::cron::Schedule::Cron {
                expr: "0 9 * * *".into(),
                tz: None,
            },
            &long_prompt,
            crate::cron::SessionTarget::Isolated,
            None,
            None,
            false,
            None,
        )
        .unwrap();

        let tool = CronListTool::new(cfg);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(
            result.output.len() < long_prompt.len(),
            "list output ({} chars) should be far shorter than the raw 5,000-char prompt",
            result.output.len()
        );
        assert!(result.output.contains("chars total, truncated"));
    }
}
