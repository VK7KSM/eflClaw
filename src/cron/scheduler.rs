use crate::config::Config;
use crate::cron::{
    due_jobs, next_run_for_schedule, record_run, remove_job, reschedule_after_run, CronJob,
    DeliveryConfig, JobType, Schedule, SessionTarget,
};
use crate::security::SecurityPolicy;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::{stream, StreamExt};
use std::sync::Arc;
use tokio::time::{self, Duration};

const MIN_POLL_SECONDS: u64 = 5;
const SCHEDULER_COMPONENT: &str = "scheduler";

// elfClaw: truncate to nearest UTF-8 char boundary (safe for CJK multi-byte)
fn truncate_str_safe(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub(crate) fn is_no_reply_sentinel(output: &str) -> bool {
    output.trim().eq_ignore_ascii_case("NO_REPLY")
}

pub async fn run(config: Config) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    loop {
        interval.tick().await;
        // Keep scheduler liveness fresh even when there are no due jobs.
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        let jobs = match due_jobs(&config, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };

        process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT).await;
    }
}

pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    Box::pin(execute_job_with_retry(config, &security, job)).await
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let (success, output) = match job.job_type {
            JobType::Agent => Box::pin(run_agent_job(config, security, job)).await,
            // elfClaw 2026-09-23: no LLM call, so there is nothing here that
            // can fail on a 429/503 — this always succeeds. `deliver_if_configured`
            // (called by `persist_job_result`) is what actually sends it.
            JobType::Message => (true, job.prompt.clone().unwrap_or_default()),
            // elfClaw 2026-09-25: code-run news push, see cron::news_pipeline.
            JobType::News => {
                let slot = job.prompt.clone().unwrap_or_default();
                match Box::pin(crate::cron::news_pipeline::run_slot(config, &slot)).await {
                    Ok(text) => (true, text),
                    Err(e) => (false, format!("news push '{slot}' failed: {e:#}")),
                }
            }
        };
        last_output = output;

        if success {
            return (true, last_output);
        }

        if last_output.starts_with("blocked by security policy:") {
            // Deterministic policy violations are not retryable.
            return (false, last_output);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output)
}

async fn process_due_jobs(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight = stream::iter(jobs.into_iter().map(|job| {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        async move {
            Box::pin(execute_and_persist_job(
                &config,
                security.as_ref(),
                &job,
                &component,
            ))
            .await
        }
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success, output)) = in_flight.next().await {
        if !success {
            tracing::warn!("Scheduler job '{job_id}' failed: {output}");
        }
    }
}

/// A job firing later than this after its due time is worth telling the
/// reader about: a catch-up run's content is stale relative to its slot.
const LATE_RUN_THRESHOLD_MINUTES: i64 = 10;

/// How late this run is, in whole minutes, once past the threshold.
///
/// `due_jobs` selects everything with `next_run <= now`, so a job whose time
/// passed while the daemon was down is caught up on the next poll rather than
/// skipped — but silently, and a "07:00 morning report" delivered at 11:00
/// reads as if nothing happened.
fn late_by_minutes(due: DateTime<Utc>, now: DateTime<Utc>) -> Option<i64> {
    let minutes = (now - due).num_minutes();
    (minutes >= LATE_RUN_THRESHOLD_MINUTES).then_some(minutes)
}

/// The note prepended to a catch-up run's output.
fn late_run_notice(minutes: i64) -> String {
    if minutes >= 120 {
        format!(
            "⏰ 本次推送迟了约 {} 小时（elfClaw 在计划时间没有运行）\n",
            minutes / 60
        )
    } else {
        format!("⏰ 本次推送迟了约 {minutes} 分钟（elfClaw 在计划时间没有运行）\n")
    }
}

async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    component: &str,
) -> (String, bool, String) {
    crate::health::mark_component_ok(component);
    warn_if_high_frequency_agent_job(job);

    let job_name = job.name.clone().unwrap_or_else(|| job.id.clone());
    // elfClaw: log cron job start
    crate::elfclaw_log::log_cron_event(
        &job.id,
        &job_name,
        "started",
        serde_json::json!({"model": &job.model}),
    );

    let started_at = Utc::now();
    let late = late_by_minutes(job.next_run, started_at);
    if let Some(minutes) = late {
        tracing::warn!(job = %job_name, late_minutes = minutes, "Cron job ran late");
    }
    let (success, mut output) = Box::pin(execute_job_with_retry(config, security, job)).await;
    if let (Some(minutes), true) = (late, success) {
        output.insert_str(0, &late_run_notice(minutes));
    }
    let finished_at = Utc::now();
    let duration_ms = (finished_at - started_at).num_milliseconds().max(0) as u64;
    let success = persist_job_result(config, job, success, &output, started_at, finished_at).await;

    // elfClaw: log cron job completion/failure
    if success {
        crate::elfclaw_log::log_cron_event(
            &job.id,
            &job_name,
            "completed",
            serde_json::json!({"duration_ms": duration_ms, "output_len": output.len()}),
        );
    } else {
        crate::elfclaw_log::log_cron_event(
            &job.id,
            &job_name,
            "failed",
            serde_json::json!({"duration_ms": duration_ms, "error": truncate_str_safe(&output, 200)}),
        );
    }

    (job.id.clone(), success, output)
}

async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }
    let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
    let prompt = job.prompt.clone().unwrap_or_default();

    // elfClaw: log cron job start so execution is visible in terminal
    tracing::info!(
        job_id = %job.id,
        job_name = %name,
        delegate_to = ?job.delegate_to,
        "Cron job starting (worker_model via RunContext::Background)"
    );

    // elfClaw: build prompt and resolve agent config for delegate_to (direct execution)
    let (prefixed_prompt, effective_allowed_tools, effective_max_iterations) = if let Some(
        ref agent_name,
    ) =
        job.delegate_to
    {
        // elfClaw: run named agent directly — no intermediate Agent #1
        // Previously this created a prompt asking Agent #1 to call delegate(agent=...),
        // wasting ~55K tokens + 2 LLM calls. Now we resolve the agent config and pass
        // allowed_tools directly to run().
        let agent_cfg = config.agents.get(agent_name);
        let allowed = agent_cfg
            .map(|c| c.allowed_tools.clone())
            .filter(|v| !v.is_empty());
        let max_iter = agent_cfg
            .map(|c| c.max_iterations)
            .unwrap_or(config.scheduler.max_tool_iterations);
        (
                format!(
                    "[cron:{id} {name}] IMPORTANT: You are a scheduled background task (agent: {agent_name}).\n\
                     \n\
                     RULES:\n\
                     1. Execute the task directly using your tools.\n\
                     2. Your final text response IS the message delivered to the user — \
                        include ALL results, summaries, and findings in it.\n\
                     3. Do NOT just say \"task completed\" or \"please check above\" — \
                        the user can ONLY see your final text response.\n\
                     4. Do NOT call send_telegram — the system delivers your response automatically.\n\
                     5. Do NOT wait for other agents — you ARE the agent responsible.\n\
                     \n\
                     Task: {prompt}",
                    id = job.id
                ),
                allowed,
                max_iter,
            )
    } else {
        // elfClaw: strong behavioral guidance for cron agents.
        // Fix 12b: explicitly instruct that final text IS the user-facing delivery,
        // prohibit empty "task completed" responses and redundant send_telegram calls.
        (
                format!(
                    "[cron:{id} {name}] IMPORTANT: You are a scheduled background task.\n\
                     \n\
                     RULES:\n\
                     1. Execute the task directly using your tools.\n\
                     2. Your final text response IS the message delivered to the user — \
                        include ALL results, summaries, and findings in it.\n\
                     3. Do NOT just say \"task completed\" or \"please check above\" — \
                        the user can ONLY see your final text response.\n\
                     4. Do NOT call send_telegram — the system delivers your response automatically.\n\
                     5. Do NOT wait for other agents — you ARE the agent responsible.\n\
                     \n\
                     Task: {prompt}",
                    id = job.id
                ),
                None,
                config.scheduler.max_tool_iterations,
            )
    };
    // elfClaw: cron jobs always use worker_model from current config, not the
    // model snapshot stored at creation time — see RunContext::Background resolution
    let model_override: Option<String> = None;

    // elfClaw 2026-09-24: collect every URL tool results returned during the
    // run; the final text is checked against them below (see
    // agent::source_links) so the model cannot deliver links it rewrote.
    let (run_result, mut source_urls) = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            crate::agent::source_links::with_ledger(Box::pin(crate::agent::run(
                config.clone(),
                Some(prefixed_prompt),
                None,
                model_override,
                config.default_temperature,
                vec![],
                false,
                Some(effective_max_iterations),
                crate::agent::RunContext::Background, // elfClaw: cron uses worker_model
                effective_allowed_tools, // elfClaw: Some(vec) for delegate_to, None otherwise
            )))
            .await
        }
    };
    // Links written into the job's own prompt are legitimate too.
    source_urls.extend(crate::agent::source_links::urls_in(&prompt));

    match run_result {
        Ok(response) => {
            let check = crate::agent::source_links::enforce(&response, &source_urls);
            if !check.repaired.is_empty() || !check.removed.is_empty() {
                tracing::warn!(
                    job_id = %job.id,
                    repaired = check.repaired.len(),
                    removed = check.removed.len(),
                    "Cron output links differed from tool results; corrected before delivery"
                );
                crate::elfclaw_log::log_cron_event(
                    &job.id,
                    &name,
                    "link_check",
                    serde_json::json!({
                        "repaired": check.repaired,
                        "removed": check.removed,
                    }),
                );
            }
            let response = check.output;
            // elfClaw: log cron job completion outcome
            if response.trim().is_empty() {
                tracing::info!(job_id = %job.id, "Cron job completed (empty output)");
                (true, "agent job executed".to_string())
            } else {
                // elfClaw: use UTF-8-safe truncation to avoid panic on CJK multi-byte chars
                tracing::info!(
                    job_id = %job.id,
                    output_preview = %truncate_str_safe(&response, 200),
                    "Cron job completed"
                );
                (true, response)
            }
        }
        Err(e) => {
            // elfClaw: log cron job failure
            tracing::warn!(job_id = %job.id, error = %e, "Cron job failed");
            (false, format!("agent job failed: {e}"))
        }
    }
}

pub(crate) async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    mut success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
) -> bool {
    let duration_ms = (finished_at - started_at).num_milliseconds();

    if let Err(e) = deliver_if_configured(config, job, output).await {
        if job.delivery.best_effort {
            tracing::warn!("Cron delivery failed (best_effort): {e}");
        } else {
            success = false;
            tracing::warn!("Cron delivery failed: {e}");
        }
    }

    // elfClaw: surface record_run errors instead of silently discarding them
    if let Err(e) = record_run(
        config,
        &job.id,
        started_at,
        finished_at,
        if success { "ok" } else { "error" },
        Some(output),
        duration_ms,
    ) {
        tracing::warn!(job_id = %job.id, error = %e, "Failed to persist cron run result");
    }

    if is_one_shot(job) {
        // elfClaw 2026-09-23: an `at` schedule always fires exactly once —
        // whatever `delete_after_run` says. It used to gate this on that flag
        // too, and a model calling cron_add could (and in production, did)
        // pass `delete_after_run: false` for a one-time reminder; the job
        // then fell through to `reschedule_after_run`, which recomputes
        // `next_run` for `Schedule::At` as *the same past timestamp* —
        // making it due again on the very next scheduler poll, forever.
        // Failure is cleaned up the same as success (not just disabled and
        // left in the list) for the same reason: a disabled one-shot with no
        // next_run served no purpose but clutter — see elfclaw.md §6.3.
        if let Err(e) = remove_job(config, &job.id) {
            tracing::warn!(
                job_id = %job.id,
                success,
                error = %e,
                "Failed to remove one-shot cron job after it ran"
            );
        }
        return success;
    }

    if let Err(e) = reschedule_after_run(config, job, success, output) {
        tracing::warn!("Failed to persist scheduler run result: {e}");
    }

    success
}

/// A job scheduled with `Schedule::At` runs exactly once, by definition —
/// independent of `delete_after_run`, which only still matters for
/// `Cron`/`Every` schedules should a future feature want "run N more times
/// then stop" semantics (nothing uses it that way today).
fn is_one_shot(job: &CronJob) -> bool {
    matches!(job.schedule, Schedule::At { .. })
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if !matches!(job.job_type, JobType::Agent) {
        return;
    }
    let too_frequent = match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            match (
                next_run_for_schedule(&job.schedule, now),
                next_run_for_schedule(&job.schedule, now + chrono::Duration::seconds(1)),
            ) {
                (Ok(a), Ok(b)) => (b - a).num_minutes() < 5,
                _ => false,
            }
        }
        Schedule::At { .. } => false,
    };

    if too_frequent {
        tracing::warn!(
            "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
            job.id
        );
    }
}

async fn deliver_if_configured(config: &Config, job: &CronJob, output: &str) -> Result<()> {
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(());
    }
    if is_no_reply_sentinel(output) {
        tracing::debug!(
            "Cron job '{}' returned NO_REPLY sentinel; skipping announce delivery",
            job.id
        );
        return Ok(());
    }

    let channel = delivery
        .channel
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.channel is required for announce mode"))?;
    let target = delivery
        .to
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.to is required for announce mode"))?;

    // elfClaw: log delivery attempt so terminal user can trace cron → channel flow
    tracing::info!(
        job_id = %job.id,
        channel = %channel,
        target = %target,
        output_len = output.len(),
        "Cron delivery: attempting announce to channel"
    );

    deliver_announcement(config, channel, target, output).await
}

pub(crate) async fn deliver_announcement(
    config: &Config,
    channel: &str,
    target: &str,
    output: &str,
) -> Result<()> {
    // Delegate to the unified channel delivery path which supports all
    // configured channels via the Channel trait, not just the original 4.
    crate::channels::deliver_to_channel(config, channel, target, output).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::cron::{self, DeliveryConfig};
    use crate::security::SecurityPolicy;
    use chrono::{Duration as ChronoDuration, Utc};
    use std::sync::OnceLock;
    use tempfile::TempDir;

    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.as_ref() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    async fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        config
    }

    // elfClaw 2026-09-23: defaults to JobType::Agent (shell jobs were removed
    // entirely) — `command` is kept populated too for the handful of tests
    // that still just need a job to exist and don't care about job_type.
    fn test_job(command: &str) -> CronJob {
        CronJob {
            id: "test-job".into(),
            expression: "* * * * *".into(),
            schedule: crate::cron::Schedule::Cron {
                expr: "* * * * *".into(),
                tz: None,
            },
            command: command.into(),
            prompt: Some(command.into()),
            name: None,
            job_type: JobType::Agent,
            session_target: SessionTarget::Isolated,
            model: None,
            delegate_to: None,
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
        }
    }

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[tokio::test]
    async fn message_job_delivers_stored_text_with_no_llm_call() {
        // elfClaw 2026-09-23: the whole point of JobType::Message is that
        // firing it can't fail on a 429/503 and doesn't touch the agent
        // loop at all — this just returns the stored prompt text directly.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let mut job = test_job("");
        job.job_type = JobType::Message;
        job.prompt = Some("带孩子看牙医".into());

        let (success, output) = execute_job_with_retry(&config, &security, &job).await;
        assert!(success);
        assert_eq!(output, "带孩子看牙医");
    }

    #[tokio::test]
    async fn run_agent_job_returns_error_without_provider_key() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let _env = env_lock().await;
        let _generic = EnvGuard::unset("ZEROCLAW_API_KEY");
        let _fallback = EnvGuard::unset("API_KEY");
        let _openrouter = EnvGuard::unset("OPENROUTER_API_KEY");
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("agent job failed:"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.max_actions_per_hour = 0;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
    }

    #[test]
    fn on_time_runs_get_no_notice() {
        let due = DateTime::parse_from_rfc3339("2026-09-26T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // The scheduler polls every few seconds, so a small lag is normal.
        assert_eq!(late_by_minutes(due, due), None);
        assert_eq!(
            late_by_minutes(due, due + chrono::Duration::minutes(9)),
            None
        );
        // A job that somehow runs early is not "late" either.
        assert_eq!(late_by_minutes(due, due - chrono::Duration::hours(1)), None);
    }

    #[test]
    fn a_catch_up_run_says_how_late_it_is() {
        let due = DateTime::parse_from_rfc3339("2026-09-26T21:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            late_by_minutes(due, due + chrono::Duration::minutes(25)),
            Some(25)
        );
        assert!(late_run_notice(25).contains("迟了约 25 分钟"));
        // Past two hours the minute count stops being readable.
        assert!(late_run_notice(245).contains("迟了约 4 小时"));
        assert!(late_run_notice(25).ends_with('\n'), "sits on its own line");
    }

    #[tokio::test]
    async fn process_due_jobs_marks_component_ok_even_when_idle() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let component = unique_component("scheduler-idle");

        crate::health::mark_component_error(&component, "pre-existing error");
        process_due_jobs(&config, &security, Vec::new(), &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
        assert!(entry["last_ok"].as_str().is_some());
        assert!(entry["last_error"].is_null());
    }

    #[tokio::test]
    async fn process_due_jobs_failure_does_not_mark_component_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_component_health_test");
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let component = unique_component("scheduler-fail");

        crate::health::mark_component_ok(&component);
        process_due_jobs(&config, &security, vec![job], &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn persist_job_result_records_run_and_reschedules_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn persist_job_result_success_deletes_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_also_deletes_one_shot() {
        // elfClaw 2026-09-23: a failed one-shot used to be disabled and left
        // in the job list forever — exactly the kind of zombie entry that
        // piled up in production (see dev_log). It's now cleaned up the same
        // as a successful run.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, false, "boom", started, finished).await;
        assert!(!success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(
            lookup.is_err(),
            "failed one-shot should be removed, not left disabled"
        );
    }

    #[tokio::test]
    async fn persist_job_result_success_deletes_one_shot_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_once_at(&config, at, "echo one-shot-shell").unwrap();
        assert!(job.delete_after_run);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_also_deletes_one_shot_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_once_at(&config, at, "echo one-shot-shell").unwrap();
        assert!(job.delete_after_run);
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, false, "boom", started, finished).await;
        assert!(!success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(
            lookup.is_err(),
            "failed one-shot should be removed, not left disabled"
        );
    }

    #[tokio::test]
    async fn persist_job_result_delivery_failure_non_best_effort_marks_error() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_agent_job(
            &config,
            Some("announce-job".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "deliver this",
            SessionTarget::Isolated,
            None,
            Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("telegram".into()),
                to: Some("123456".into()),
                best_effort: false,
            }),
            false,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(!success);

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("error"));

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "error");
    }

    #[tokio::test]
    async fn persist_job_result_delivery_failure_best_effort_keeps_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_agent_job(
            &config,
            Some("announce-job-best-effort".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "deliver this",
            SessionTarget::Isolated,
            None,
            Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("telegram".into()),
                to: Some("123456".into()),
                best_effort: true,
            }),
            false,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);

        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("ok"));

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");
    }

    #[tokio::test]
    async fn persist_job_result_at_schedule_is_deleted_even_with_delete_after_run_false() {
        // elfClaw 2026-09-23: this pins the actual production bug. A model
        // calling cron_add can pass `delete_after_run: false` for a one-time
        // reminder (weak models default unfamiliar booleans to false); the
        // old code trusted that flag and rescheduled the job instead of
        // deleting it. `next_run_for_schedule(Schedule::At{at})` just returns
        // the same past `at` again, so the job became due on every following
        // scheduler poll forever — a reminder that never stops re-firing.
        // `Schedule::At` must always mean "runs once," full stop, regardless
        // of what the caller set `delete_after_run` to.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("at-explicit-false".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(!job.delete_after_run);

        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let success = persist_job_result(&config, &job, true, "ok", started, finished).await;
        assert!(success);

        let lookup = cron::get_job(&config, &job.id);
        assert!(
            lookup.is_err(),
            "an `at` job must be deleted after it runs even when delete_after_run=false"
        );
    }

    #[tokio::test]
    async fn deliver_if_configured_handles_none_and_invalid_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("echo ok");

        assert!(deliver_if_configured(&config, &job, "x").await.is_ok());

        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("invalid".into()),
            to: Some("target".into()),
            best_effort: true,
        };
        let err = deliver_if_configured(&config, &job, "x").await.unwrap_err();
        assert!(
            err.to_string().contains("unsupported delivery channel")
                || err.to_string().contains("no channel named")
        );
    }

    #[tokio::test]
    async fn deliver_if_configured_skips_no_reply_sentinel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("echo ok");
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("invalid".into()),
            to: Some("target".into()),
            best_effort: true,
        };

        assert!(deliver_if_configured(&config, &job, "  no_reply  ")
            .await
            .is_ok());
    }

    #[test]
    fn no_reply_sentinel_matching_is_trimmed_and_case_insensitive() {
        assert!(is_no_reply_sentinel("NO_REPLY"));
        assert!(is_no_reply_sentinel("  no_reply  "));
        assert!(!is_no_reply_sentinel("NO_REPLY please"));
        assert!(!is_no_reply_sentinel(""));
    }

    #[tokio::test]
    async fn deliver_if_configured_whatsapp_web_requires_live_session_in_web_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.channels_config.whatsapp = Some(crate::config::schema::WhatsAppConfig {
            access_token: None,
            phone_number_id: None,
            verify_token: None,
            app_secret: None,
            session_path: Some("~/.zeroclaw/state/whatsapp-web/session.db".into()),
            pair_phone: None,
            pair_code: None,
            allowed_numbers: vec!["*".into()],
        });

        let mut job = test_job("echo ok");
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("whatsapp_web".into()),
            to: Some("+15551234567".into()),
            best_effort: true,
        };

        let err = deliver_if_configured(&config, &job, "x").await.unwrap_err();
        assert!(err
            .to_string()
            .contains("requires an active channels runtime session"));
    }
}
