use crate::cron::Schedule;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cron::Schedule as CronExprSchedule;
use std::str::FromStr;

pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let normalized = normalize_expression(expr)?;
            let cron = CronExprSchedule::from_str(&normalized)
                .with_context(|| format!("Invalid cron expression: {expr}"))?;

            if let Some(tz_name) = tz {
                let timezone = chrono_tz::Tz::from_str(tz_name)
                    .with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
                let localized_from = from.with_timezone(&timezone);
                let next_local = cron.after(&localized_from).next().ok_or_else(|| {
                    anyhow::anyhow!("No future occurrence for expression: {expr}")
                })?;
                Ok(next_local.with_timezone(&Utc))
            } else {
                cron.after(&from)
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))
            }
        }
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = ChronoDuration::milliseconds(ms);
            from.checked_add_signed(delta)
                .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
        }
    }
}

/// elfClaw 2026-09-23: apply `config.cron.default_tz` to a `Schedule::Cron`
/// that doesn't specify its own `tz`. Jobs created without an explicit
/// timezone used to run on the server's UTC clock ("daily 8am" firing at
/// 18:00/19:00 Sydney time) — see elfclaw.md §6. Call this once, as early as
/// possible (job creation/update), so `next_run_for_schedule` itself never
/// has to know about config.
pub fn apply_default_tz(config: &crate::config::Config, schedule: Schedule) -> Schedule {
    match schedule {
        Schedule::Cron { expr, tz: None } if config.cron.default_tz.is_some() => Schedule::Cron {
            expr,
            tz: config.cron.default_tz.clone(),
        },
        other => other,
    }
}

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                anyhow::bail!("Invalid schedule: 'at' must be in the future");
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        5 => Ok(format!("0 {expression}")),
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!(
            "Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn apply_default_tz_fills_in_sydney_by_default() {
        let config = crate::config::Config::default();
        assert_eq!(
            config.cron.default_tz.as_deref(),
            Some("Australia/Sydney"),
            "precondition: default config should default to Sydney"
        );

        let schedule = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: None,
        };
        let result = apply_default_tz(&config, schedule);
        assert_eq!(
            result,
            Schedule::Cron {
                expr: "0 8 * * *".into(),
                tz: Some("Australia/Sydney".into()),
            }
        );
    }

    #[test]
    fn apply_default_tz_does_not_override_an_explicit_tz() {
        let config = crate::config::Config::default();
        let schedule = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };
        let result = apply_default_tz(&config, schedule.clone());
        assert_eq!(result, schedule, "an explicit tz must win over the default");
    }

    #[test]
    fn apply_default_tz_leaves_at_and_every_schedules_untouched() {
        let config = crate::config::Config::default();
        let at = Schedule::At { at: Utc::now() };
        assert_eq!(apply_default_tz(&config, at.clone()), at);
        let every = Schedule::Every { every_ms: 60_000 };
        assert_eq!(apply_default_tz(&config, every.clone()), every);
    }

    #[test]
    fn apply_default_tz_respects_none_meaning_keep_utc() {
        let mut config = crate::config::Config::default();
        config.cron.default_tz = None;
        let schedule = Schedule::Cron {
            expr: "0 8 * * *".into(),
            tz: None,
        };
        let result = apply_default_tz(&config, schedule.clone());
        assert_eq!(
            result, schedule,
            "default_tz=None should keep the old UTC behavior"
        );
    }

    #[test]
    fn next_run_for_schedule_supports_every_and_at() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);

        let at = now + ChronoDuration::minutes(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_for_schedule_supports_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };

        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
    }
}
