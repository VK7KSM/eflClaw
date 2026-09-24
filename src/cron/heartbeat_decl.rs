//! elfClaw 2026-09-23: code-driven reconciliation for `HEARTBEAT.md`.
//!
//! Before this, the daemon's heartbeat worker sent the *entire* HEARTBEAT.md
//! file to a weak LLM every 30 minutes with a prompt asking it to compare
//! against `cron_list` and `cron_add` anything missing. In production this
//! produced 22 duplicate/near-duplicate cron jobs on one instance (7 copies
//! of the same "news search" job alone, each under a slightly different
//! name) — the model would misjudge a truncated `cron_list` result, or pick
//! a slightly different name each time, and there was no reliable way for it
//! to tell "already exists" from "doesn't exist yet".
//!
//! This module replaces that LLM round-trip for *scheduled, recurring*
//! HEARTBEAT.md entries with a small parser + a deterministic reconcile pass
//! that calls straight into `cron::add_agent_job`, which is idempotent by
//! name (see `cron::store::add_agent_job`). No model call, no ambiguity.
//!
//! ## Format
//!
//! A task is declared as a `<!-- heartbeat-task ... -->` HTML-comment block
//! containing TOML, so it's invisible when the file is rendered as Markdown
//! and doesn't interfere with any free-form prose elsewhere in the file:
//!
//! ```text
//! <!-- heartbeat-task
//! name = "早报综合"
//! schedule = { kind = "cron", expr = "30 6 * * *", tz = "Australia/Sydney" }
//! prompt = "Fetch and summarize this morning's news."
//! delivery = { mode = "announce", channel = "telegram", to = "495916105" }
//! -->
//! ```
//!
//! `schedule`/`delivery` reuse the exact same [`crate::cron::Schedule`] /
//! [`crate::cron::DeliveryConfig`] types the `cron_add` tool already accepts
//! — same TOML shape as their JSON shape, just TOML instead of JSON.
//!
//! Optional `delegate_to = "<agent>"` runs the task directly as that
//! `[agents.<agent>]` sub-agent (its own `allowed_tools`/`max_iterations`),
//! skipping the main agent. The name must exist in config — the scheduler
//! would otherwise fall back to running with every tool.
//!
//! Declared jobs are stored with their name prefixed `heartbeat:` (e.g.
//! `heartbeat:早报综合`) so reconciliation can always tell a
//! HEARTBEAT.md-managed job apart from one a user asked the chat agent to
//! create ad hoc via `cron_add` — reconciliation only ever creates, updates,
//! or removes jobs under that prefix. Removing a task's block from
//! HEARTBEAT.md removes the corresponding cron job on the next tick.

use crate::config::Config;
use crate::cron::{self, DeliveryConfig, Schedule, SessionTarget};
use anyhow::{Context, Result};
use std::collections::HashSet;

/// Cron jobs created from HEARTBEAT.md are namespaced under this name prefix
/// so reconciliation never touches a job the user (or the chat agent, on the
/// user's behalf) created directly.
pub const MANAGED_NAME_PREFIX: &str = "heartbeat:";

/// Recurring `every` schedules declared in HEARTBEAT.md must be at least
/// this often — same floor the `cron_add` tool enforces for agent jobs, so a
/// config typo (e.g. `every_ms = 1000`) can't turn into a runaway LLM loop.
const MIN_EVERY_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct HeartbeatTaskDecl {
    pub name: String,
    pub schedule: Schedule,
    pub prompt: String,
    #[serde(default)]
    pub delivery: Option<DeliveryConfig>,
    #[serde(default)]
    pub delegate_to: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub updated: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

impl ReconcileReport {
    pub fn total_changes(&self) -> usize {
        self.created.len() + self.updated.len() + self.removed.len()
    }
}

/// Extract every `<!-- heartbeat-task ... -->` block from `content` and parse
/// it as TOML into a [`HeartbeatTaskDecl`]. A block that fails to parse is
/// reported as an error string (block index + reason) rather than silently
/// dropped or aborting the whole file — one typo shouldn't take down every
/// other declared task.
pub fn parse_heartbeat_task_declarations(content: &str) -> (Vec<HeartbeatTaskDecl>, Vec<String>) {
    const OPEN: &str = "<!-- heartbeat-task";
    const CLOSE: &str = "-->";

    let mut decls = Vec::new();
    let mut errors = Vec::new();
    let mut search_from = 0usize;
    let mut block_index = 0usize;

    while let Some(open_rel) = content[search_from..].find(OPEN) {
        let open_abs = search_from + open_rel;
        let body_start = open_abs + OPEN.len();
        block_index += 1;

        let Some(close_rel) = content[body_start..].find(CLOSE) else {
            errors.push(format!(
                "heartbeat-task block #{block_index}: missing closing '-->'"
            ));
            break;
        };
        let body_end = body_start + close_rel;
        let body = &content[body_start..body_end];

        match toml::from_str::<HeartbeatTaskDecl>(body) {
            Ok(decl) if decl.name.trim().is_empty() => {
                errors.push(format!(
                    "heartbeat-task block #{block_index}: 'name' must not be empty"
                ));
            }
            Ok(decl) => decls.push(decl),
            Err(e) => errors.push(format!("heartbeat-task block #{block_index}: {e}")),
        }

        search_from = body_end + CLOSE.len();
    }

    // Duplicate names within the same file would just overwrite each other
    // on every tick (last one wins) — surface that as a diagnostic instead
    // of letting it happen silently.
    let mut seen = HashSet::new();
    for decl in &decls {
        if !seen.insert(decl.name.as_str()) {
            errors.push(format!(
                "heartbeat-task '{}' is declared more than once; only the last occurrence takes effect",
                decl.name
            ));
        }
    }

    (decls, errors)
}

/// Reconcile the cron jobs table against `declared`: create any missing
/// job, update any that changed, and remove any `heartbeat:`-managed job
/// that is no longer declared. Idempotent — safe to call on every heartbeat
/// tick and at daemon startup.
pub fn reconcile(config: &Config, declared: &[HeartbeatTaskDecl]) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let mut declared_managed_names: HashSet<String> = HashSet::new();

    for decl in declared {
        let managed_name = format!("{MANAGED_NAME_PREFIX}{}", decl.name);
        declared_managed_names.insert(managed_name.clone());

        if let Schedule::Every { every_ms } = decl.schedule {
            if every_ms < MIN_EVERY_MS {
                report.errors.push(format!(
                    "heartbeat-task '{}': every_ms={every_ms} is below the {MIN_EVERY_MS}ms floor; skipped",
                    decl.name
                ));
                continue;
            }
        }

        if let Some(agent) = decl.delegate_to.as_deref() {
            if !config.agents.contains_key(agent) {
                report.errors.push(format!(
                    "heartbeat-task '{}': delegate_to agent '{agent}' is not defined in [agents]; skipped",
                    decl.name
                ));
                continue;
            }
        }

        let existing = cron::list_jobs(config).ok().and_then(|jobs| {
            jobs.into_iter()
                .find(|j| j.name.as_deref() == Some(managed_name.as_str()))
        });
        let existed_before = existing.is_some();

        // add_agent_job's update path treats `delegate_to: None` as "leave
        // unchanged", so dropping delegate_to from a block would otherwise
        // keep delegating forever. Recreate the job in that case.
        if let Some(job) = &existing {
            if job.delegate_to.is_some() && decl.delegate_to.is_none() {
                if let Err(e) = cron::remove_job(config, &job.id) {
                    report
                        .errors
                        .push(format!("heartbeat-task '{}': {e}", decl.name));
                    continue;
                }
            }
        }

        match cron::add_agent_job(
            config,
            Some(managed_name.clone()),
            decl.schedule.clone(),
            &decl.prompt,
            SessionTarget::Isolated,
            None,
            decl.delivery.clone(),
            false,
            decl.delegate_to.clone(),
        ) {
            Ok(_job) => {
                if existed_before {
                    report.updated.push(decl.name.clone());
                } else {
                    report.created.push(decl.name.clone());
                }
            }
            Err(e) => report
                .errors
                .push(format!("heartbeat-task '{}': {e}", decl.name)),
        }
    }

    let existing = cron::list_jobs(config).context("listing cron jobs during reconcile")?;
    for job in existing {
        let Some(name) = job.name.as_deref() else {
            continue;
        };
        if !name.starts_with(MANAGED_NAME_PREFIX) {
            continue; // not ours — never touch a user/ad-hoc job
        }
        if declared_managed_names.contains(name) {
            continue; // still declared
        }
        // Managed but no longer declared: the block was removed (or
        // renamed) from HEARTBEAT.md — remove the stale job rather than
        // leaving a zombie entry behind.
        match cron::remove_job(config, &job.id) {
            Ok(()) => report.removed.push(name.to_string()),
            Err(e) => report
                .errors
                .push(format!("removing stale heartbeat job '{name}': {e}")),
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_task_block() {
        let content = r#"
# Periodic Tasks

<!-- heartbeat-task
name = "morning-news"
schedule = { kind = "cron", expr = "30 6 * * *", tz = "Australia/Sydney" }
prompt = "Fetch and summarize this morning's news."
delivery = { mode = "announce", channel = "telegram", to = "495916105" }
-->

Some free-form notes a human wrote here that the parser should ignore.
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].name, "morning-news");
        assert_eq!(decls[0].prompt, "Fetch and summarize this morning's news.");
        assert!(matches!(decls[0].schedule, Schedule::Cron { .. }));
        assert_eq!(
            decls[0].delivery.as_ref().unwrap().channel.as_deref(),
            Some("telegram")
        );
    }

    #[test]
    fn parses_multiple_task_blocks() {
        let content = r#"
<!-- heartbeat-task
name = "a"
schedule = { kind = "cron", expr = "0 9 * * *" }
prompt = "Task A"
-->

<!-- heartbeat-task
name = "b"
schedule = { kind = "every", every_ms = 1800000 }
prompt = "Task B"
-->
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "a");
        assert_eq!(decls[1].name, "b");
    }

    #[test]
    fn no_blocks_returns_empty_without_error() {
        let content = "# Periodic Tasks\n\nJust free-form prose, no declared tasks.\n";
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert!(decls.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn malformed_block_reports_error_without_aborting_the_rest() {
        let content = r#"
<!-- heartbeat-task
name = "broken
this is not valid toml at all [[[
-->

<!-- heartbeat-task
name = "still-works"
schedule = { kind = "cron", expr = "0 9 * * *" }
prompt = "Task"
-->
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert_eq!(decls.len(), 1, "the well-formed block should still parse");
        assert_eq!(decls[0].name, "still-works");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("block #1"));
    }

    #[test]
    fn empty_name_is_rejected() {
        let content = r#"
<!-- heartbeat-task
name = ""
schedule = { kind = "cron", expr = "0 9 * * *" }
prompt = "Task"
-->
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert!(decls.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("must not be empty"));
    }

    #[test]
    fn duplicate_name_is_flagged() {
        let content = r#"
<!-- heartbeat-task
name = "dup"
schedule = { kind = "cron", expr = "0 9 * * *" }
prompt = "First"
-->

<!-- heartbeat-task
name = "dup"
schedule = { kind = "cron", expr = "0 10 * * *" }
prompt = "Second"
-->
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert_eq!(decls.len(), 2);
        assert!(errors.iter().any(|e| e.contains("declared more than once")));
    }

    // ── Reconciliation ──

    use crate::config::Config;
    use tempfile::TempDir;

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

    #[tokio::test]
    async fn reconcile_creates_declared_jobs_under_managed_prefix() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let decls = vec![HeartbeatTaskDecl {
            name: "morning-news".into(),
            schedule: Schedule::Cron {
                expr: "30 6 * * *".into(),
                tz: Some("Australia/Sydney".into()),
            },
            prompt: "Summarize the news".into(),
            delivery: None,
            delegate_to: None,
        }];

        let report = reconcile(&config, &decls).unwrap();
        assert_eq!(report.created, vec!["morning-news"]);
        assert!(report.errors.is_empty());

        let jobs = cron::list_jobs(&config).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name.as_deref(), Some("heartbeat:morning-news"));
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_second_call_updates_not_duplicates() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let decls = vec![HeartbeatTaskDecl {
            name: "morning-news".into(),
            schedule: Schedule::Cron {
                expr: "30 6 * * *".into(),
                tz: None,
            },
            prompt: "v1".into(),
            delivery: None,
            delegate_to: None,
        }];
        reconcile(&config, &decls).unwrap();

        let decls_v2 = vec![HeartbeatTaskDecl {
            name: "morning-news".into(),
            schedule: Schedule::Cron {
                expr: "0 7 * * *".into(),
                tz: None,
            },
            prompt: "v2".into(),
            delivery: None,
            delegate_to: None,
        }];
        let report = reconcile(&config, &decls_v2).unwrap();
        assert_eq!(report.updated, vec!["morning-news"]);
        assert!(report.created.is_empty());

        let jobs = cron::list_jobs(&config).unwrap();
        assert_eq!(
            jobs.len(),
            1,
            "must update in place, not create a duplicate"
        );
        assert_eq!(jobs[0].expression, "0 7 * * *");
        assert_eq!(jobs[0].prompt.as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn reconcile_removes_job_whose_block_was_deleted() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let decls = vec![HeartbeatTaskDecl {
            name: "temp-task".into(),
            schedule: Schedule::Cron {
                expr: "0 9 * * *".into(),
                tz: None,
            },
            prompt: "temp".into(),
            delivery: None,
            delegate_to: None,
        }];
        reconcile(&config, &decls).unwrap();
        assert_eq!(cron::list_jobs(&config).unwrap().len(), 1);

        let report = reconcile(&config, &[]).unwrap();
        assert_eq!(report.removed, vec!["heartbeat:temp-task"]);
        assert!(cron::list_jobs(&config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconcile_never_touches_a_job_outside_the_managed_prefix() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        // A job the chat agent created ad hoc via cron_add — no "heartbeat:" prefix.
        cron::add_agent_job(
            &config,
            Some("user-reminder".into()),
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::minutes(30),
            },
            "remind me",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
        )
        .unwrap();

        // Reconciling an empty declared set must not remove it.
        let report = reconcile(&config, &[]).unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(cron::list_jobs(&config).unwrap().len(), 1);
    }

    fn with_news_fetcher_agent(mut config: Config) -> Config {
        let agent: crate::config::DelegateAgentConfig =
            toml::from_str(r#"allowed_tools = ["file_read"]"#).unwrap();
        config.agents.insert("news_fetcher".into(), agent);
        config
    }

    fn news_decl(delegate_to: Option<&str>) -> HeartbeatTaskDecl {
        HeartbeatTaskDecl {
            name: "morning-news".into(),
            schedule: Schedule::Cron {
                expr: "30 6 * * *".into(),
                tz: None,
            },
            prompt: "fetch".into(),
            delivery: None,
            delegate_to: delegate_to.map(str::to_string),
        }
    }

    #[test]
    fn parses_delegate_to_field() {
        let content = r#"
<!-- heartbeat-task
name = "news"
schedule = { kind = "cron", expr = "30 6 * * *" }
prompt = "fetch"
delegate_to = "news_fetcher"
-->
"#;
        let (decls, errors) = parse_heartbeat_task_declarations(content);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(decls[0].delegate_to.as_deref(), Some("news_fetcher"));
    }

    #[tokio::test]
    async fn reconcile_passes_delegate_to_through_to_the_job() {
        let tmp = TempDir::new().unwrap();
        let config = with_news_fetcher_agent(test_config(&tmp).await);

        let report = reconcile(&config, &[news_decl(Some("news_fetcher"))]).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let jobs = cron::list_jobs(&config).unwrap();
        assert_eq!(jobs[0].delegate_to.as_deref(), Some("news_fetcher"));
    }

    #[tokio::test]
    async fn reconcile_skips_unknown_delegate_agent() {
        // The scheduler runs an unknown delegate_to agent with *every* tool,
        // so an undefined name must never reach the jobs table.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;

        let report = reconcile(&config, &[news_decl(Some("no_such_agent"))]).unwrap();
        assert!(report.created.is_empty());
        assert!(report.errors.iter().any(|e| e.contains("no_such_agent")));
        assert!(cron::list_jobs(&config).unwrap().is_empty());
    }

    #[tokio::test]
    async fn reconcile_clears_delegate_to_when_removed_from_block() {
        let tmp = TempDir::new().unwrap();
        let config = with_news_fetcher_agent(test_config(&tmp).await);
        reconcile(&config, &[news_decl(Some("news_fetcher"))]).unwrap();

        reconcile(&config, &[news_decl(None)]).unwrap();
        let jobs = cron::list_jobs(&config).unwrap();
        assert_eq!(jobs.len(), 1, "must not leave a duplicate behind");
        assert_eq!(jobs[0].delegate_to, None);
    }

    #[tokio::test]
    async fn reconcile_rejects_every_schedule_below_minimum_interval() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let decls = vec![HeartbeatTaskDecl {
            name: "too-frequent".into(),
            schedule: Schedule::Every { every_ms: 1_000 },
            prompt: "spam".into(),
            delivery: None,
            delegate_to: None,
        }];

        let report = reconcile(&config, &decls).unwrap();
        assert!(report.created.is_empty());
        assert!(report.errors.iter().any(|e| e.contains("below")));
        assert!(cron::list_jobs(&config).unwrap().is_empty());
    }
}
