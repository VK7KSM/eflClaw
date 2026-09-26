//! elfClaw 2026-09-26: local accounting of free-tier model usage.
//!
//! The Gemini API has no endpoint that reports remaining quota — `ListModels`
//! returns model metadata only, and Google Cloud's quota API needs OAuth plus
//! a GCP project rather than an API key. So usage is counted here, from the
//! calls elfClaw itself makes. It costs nothing and consumes no quota.
//!
//! Two things are recorded per key and model:
//!
//! - every successful call, grouped by *quota day*;
//! - the moment a key runs out for the day (a daily-quota 429). The number of
//!   successful calls made before that point is the **observed** daily limit,
//!   so nothing has to hardcode Google's per-model numbers, which change.
//!
//! Google's free tier resets at midnight America/Los_Angeles, so that is the
//! day boundary used for grouping. It is never shown to the reader: the reset
//! time is rendered in the reader's own timezone.

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

/// Where Google's free tier rolls over.
const QUOTA_TZ: chrono_tz::Tz = chrono_tz::America::Los_Angeles;
/// Rows older than this are dropped on write.
const KEEP_DAYS: u64 = 14;
/// Models listed in the summary line, busiest first.
const MAX_MODELS_SHOWN: usize = 3;

static STORE: LazyLock<RwLock<Option<Arc<QuotaStore>>>> = LazyLock::new(|| RwLock::new(None));

/// The date whose quota a call at `now` counts against.
pub fn quota_day(now: DateTime<Utc>) -> String {
    now.with_timezone(&QUOTA_TZ).date_naive().to_string()
}

/// When the current quota day rolls over, as `HH:MM` in `tz`.
///
/// Callers render this without naming the timezone — the reader sees their own
/// clock, which is the only part that is useful to them.
pub fn reset_label(now: DateTime<Utc>, tz: &str) -> Option<String> {
    let tomorrow = now.with_timezone(&QUOTA_TZ).date_naive() + chrono::Days::new(1);
    let midnight = QUOTA_TZ
        .from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0)?)
        .earliest()?;
    let local = tz.parse::<chrono_tz::Tz>().ok()?;
    Some(midnight.with_timezone(&local).format("%H:%M").to_string())
}

// ── store ────────────────────────────────────────────────────────────────

struct QuotaStore {
    conn: parking_lot::Mutex<rusqlite::Connection>,
}

impl QuotaStore {
    fn init(workspace_dir: &Path) -> Result<Self> {
        let path = workspace_dir.join("state").join("quota.db");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage (
                 day TEXT NOT NULL,
                 label TEXT NOT NULL,
                 model TEXT NOT NULL,
                 calls INTEGER NOT NULL DEFAULT 0,
                 exhausted_at TEXT,
                 PRIMARY KEY (day, label, model)
             );
             CREATE TABLE IF NOT EXISTS observed_limit (
                 model TEXT PRIMARY KEY,
                 calls INTEGER NOT NULL,
                 seen_on TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS model_check (
                 day TEXT PRIMARY KEY,
                 missing TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            conn: parking_lot::Mutex::new(conn),
        })
    }

    fn record_call(&self, label: &str, model: &str, now: DateTime<Utc>) {
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT INTO usage (day, label, model, calls) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(day, label, model) DO UPDATE SET calls = calls + 1",
            rusqlite::params![quota_day(now), label, model],
        );
    }

    fn record_exhausted(&self, label: &str, model: &str, now: DateTime<Utc>) {
        let day = quota_day(now);
        let stamp = now.to_rfc3339();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT INTO usage (day, label, model, calls, exhausted_at) VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(day, label, model) DO UPDATE SET exhausted_at = COALESCE(exhausted_at, ?4)",
            rusqlite::params![day, label, model, stamp],
        );
        let calls: i64 = conn
            .query_row(
                "SELECT calls FROM usage WHERE day = ?1 AND label = ?2 AND model = ?3",
                rusqlite::params![day, label, model],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if calls > 0 {
            let _ = conn.execute(
                "INSERT INTO observed_limit (model, calls, seen_on) VALUES (?1, ?2, ?3)
                 ON CONFLICT(model) DO UPDATE SET calls = MAX(calls, ?2), seen_on = ?3",
                rusqlite::params![model, calls, day],
            );
        }
        let cutoff = now.with_timezone(&QUOTA_TZ).date_naive() - chrono::Days::new(KEEP_DAYS);
        let _ = conn.execute("DELETE FROM usage WHERE day < ?1", [cutoff.to_string()]);
    }

    /// The result of today's availability check, if it already ran.
    fn model_check(&self, day: &str) -> Option<String> {
        self.conn
            .lock()
            .query_row(
                "SELECT missing FROM model_check WHERE day = ?1",
                [day],
                |r| r.get(0),
            )
            .ok()
    }

    fn set_model_check(&self, day: &str, missing: &str) {
        let _ = self.conn.lock().execute(
            "INSERT OR REPLACE INTO model_check (day, missing) VALUES (?1, ?2)",
            rusqlite::params![day, missing],
        );
    }

    fn today(&self, now: DateTime<Utc>) -> Vec<ModelUsage> {
        let conn = self.conn.lock();
        let Ok(mut stmt) = conn.prepare(
            "SELECT u.model,
                    SUM(u.calls),
                    COUNT(*),
                    SUM(CASE WHEN u.exhausted_at IS NOT NULL THEN 1 ELSE 0 END),
                    (SELECT calls FROM observed_limit o WHERE o.model = u.model)
             FROM usage u WHERE u.day = ?1
             GROUP BY u.model ORDER BY SUM(u.calls) DESC",
        ) else {
            return Vec::new();
        };
        let rows = stmt.query_map([quota_day(now)], |r| {
            Ok(ModelUsage {
                model: r.get(0)?,
                calls: r.get(1)?,
                keys_seen: usize::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
                keys_exhausted: usize::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
                observed_limit: r.get(4)?,
            })
        });
        rows.map(|r| r.filter_map(std::result::Result::ok).collect())
            .unwrap_or_default()
    }
}

fn store() -> Option<Arc<QuotaStore>> {
    STORE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Open the usage database. Call once at startup; failures are logged and
/// leave accounting disabled rather than stopping the daemon.
pub fn init(workspace_dir: &Path) {
    match QuotaStore::init(workspace_dir) {
        Ok(s) => {
            let mut guard = STORE
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(Arc::new(s));
        }
        Err(e) => tracing::warn!("quota: failed to open usage database: {e}"),
    }
}

/// Count one successful call against `label` (the per-key provider name, e.g.
/// `gemini#2`). Best-effort: never fails a request.
pub fn record_call(label: &str, model: &str) {
    if let Some(store) = store() {
        store.record_call(label, model, Utc::now());
    }
}

/// Record that `label` ran out of daily quota for `model`. The calls it made
/// beforehand become the observed limit, keeping the largest seen so far —
/// a key that was already partly used on an earlier day would understate it.
pub fn record_exhausted(label: &str, model: &str) {
    if let Some(store) = store() {
        store.record_exhausted(label, model, Utc::now());
    }
}

// ── summary ──────────────────────────────────────────────────────────────

/// Today's usage of one model across every key.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub model: String,
    pub calls: i64,
    /// Keys that made at least one call, or ran out, today.
    pub keys_seen: usize,
    pub keys_exhausted: usize,
    /// Largest number of calls any key made before running out; `None` until
    /// a key has actually run out at least once.
    pub observed_limit: Option<i64>,
}

impl ModelUsage {
    /// Estimated calls left today across `keys` keys, when a limit is known.
    pub fn remaining(&self, keys: usize) -> Option<i64> {
        let limit = self.observed_limit?;
        Some((limit * i64::try_from(keys).unwrap_or(1) - self.calls).max(0))
    }
}

/// Today's usage, busiest model first.
pub fn today(now: DateTime<Utc>) -> Vec<ModelUsage> {
    store().map(|s| s.today(now)).unwrap_or_default()
}

/// Render the note. Split from `summary_line` so it can be checked without
/// touching the process-wide store.
///
/// Every number is spelled out, because a bare "remaining" figure says nothing
/// on its own: a model whose ceiling has been observed shows `used/total` plus
/// how that total was derived, one whose ceiling is still unknown shows the
/// count and says so, and models that were not called at all are listed too —
/// otherwise there is no way to tell "untouched" from "not tracked".
fn render_summary(
    usage: &[ModelUsage],
    configured: &[String],
    keys: usize,
    now: DateTime<Utc>,
    tz: &str,
) -> Option<String> {
    if usage.is_empty() {
        return None;
    }
    let keys = keys.max(1);
    let short = |m: &str| m.strip_prefix("gemini-").unwrap_or(m).to_string();
    let mut lines = vec![match reset_label(now, tz) {
        Some(reset) => format!("🔑 今日用量（{reset} 重置）"),
        None => "🔑 今日用量".to_string(),
    }];

    for u in usage.iter().filter(|u| u.observed_limit.is_some()) {
        let Some(limit) = u.observed_limit else {
            continue;
        };
        let total = limit * i64::try_from(keys).unwrap_or(1);
        let mut line = format!(
            "• {} {}/约 {total}（{keys} 个 key × 单 key 约 {limit}）",
            short(&u.model),
            u.calls
        );
        if u.keys_exhausted > 0 {
            let _ = std::fmt::Write::write_fmt(
                &mut line,
                format_args!("，{} 个 key 已用完", u.keys_exhausted),
            );
        }
        lines.push(line);
    }

    let unknown: Vec<String> = usage
        .iter()
        .filter(|u| u.observed_limit.is_none())
        .map(|u| format!("{} {}", short(&u.model), u.calls))
        .collect();
    if !unknown.is_empty() {
        lines.push(format!(
            "• {}（还没有 key 触顶，上限未知）",
            unknown.join("、")
        ));
    }

    let used: std::collections::HashSet<&str> = usage.iter().map(|u| u.model.as_str()).collect();
    let unused: Vec<String> = configured
        .iter()
        .filter(|m| !used.contains(m.as_str()))
        .map(|m| short(m))
        .collect();
    if !unused.is_empty() {
        lines.push(format!("• 今天没用到：{}", unused.join("、")));
    }
    Some(lines.join("\n"))
}

/// The quota note appended to every push. `keys` is how many API keys are
/// configured, `configured` the models elfClaw might call, `tz` the reader's
/// timezone. Returns `None` before any call has been recorded today.
pub fn summary_line(
    now: DateTime<Utc>,
    keys: usize,
    configured: &[String],
    tz: &str,
) -> Option<String> {
    render_summary(&today(now), configured, keys, now, tz)
}

// ── model availability ───────────────────────────────────────────────────
//
// Google retires model names over time. The endpoint itself never changes —
// only the name in the path — so a retirement needs a config edit, not a code
// change. `ListModels` is metadata and costs no generation quota, so it is
// asked once a day and the answer cached; anything configured but no longer
// listed is reported in the push rather than silently swapped, because a
// different model has different speed, quality and limits.

const LIST_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const LIST_MODELS_TIMEOUT_SECS: u64 = 15;

/// Every Gemini model name elfClaw might send, from config.
pub fn configured_models(config: &crate::config::Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |m: Option<&str>| {
        if let Some(m) = m {
            let m = m.trim();
            if m.starts_with("gemini") && !out.iter().any(|x| x == m) {
                out.push(m.to_string());
            }
        }
    };
    add(config.default_model.as_deref());
    add(config.summary_model.as_deref());
    add(config.worker_model.as_deref());
    for (from, to) in &config.reliability.model_fallbacks {
        add(Some(from.as_str()));
        for m in to {
            add(Some(m.as_str()));
        }
    }
    out
}

/// Names the API currently lists. One metadata request.
async fn list_models(api_key: &str) -> Option<std::collections::HashSet<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(LIST_MODELS_TIMEOUT_SECS))
        .build()
        .ok()?;
    let body: serde_json::Value = client
        .get(format!("{LIST_MODELS_URL}?pageSize=200"))
        .header("x-goog-api-key", api_key)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let models = body.get("models")?.as_array()?;
    // Entries look like "models/gemini-3.5-flash".
    Some(
        models
            .iter()
            .filter_map(|m| m.get("name")?.as_str())
            .filter_map(|n| n.rsplit('/').next())
            .map(str::to_string)
            .collect(),
    )
}

/// Configured models the API no longer lists, given what it does list.
pub fn retired<S: std::hash::BuildHasher>(
    configured: &[String],
    available: &std::collections::HashSet<String, S>,
) -> Vec<String> {
    configured
        .iter()
        .filter(|m| !available.contains(*m))
        .cloned()
        .collect()
}

/// A warning line when a configured model has been retired, checked at most
/// once per quota day. `None` when everything is still available, when the
/// check already ran today with nothing missing, or when it could not run.
pub async fn retired_model_warning(
    config: &crate::config::Config,
    now: DateTime<Utc>,
) -> Option<String> {
    let store = store()?;
    let day = quota_day(now);
    if let Some(cached) = store.model_check(&day) {
        return (!cached.is_empty()).then(|| format!("⚠️ 模型已下线：{cached}，需要改配置"));
    }
    let configured = configured_models(config);
    if configured.is_empty() {
        return None;
    }
    let key = config.api_key.as_deref()?;
    let available = list_models(key).await?;
    let missing = retired(&configured, &available);
    store.set_model_check(&day, &missing.join("、"));
    (!missing.is_empty()).then(|| format!("⚠️ 模型已下线：{}，需要改配置", missing.join("、")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// A store of its own per test — the process-wide one is never touched, so
    /// these stay independent under the parallel test runner.
    fn store_in(dir: &tempfile::TempDir) -> QuotaStore {
        QuotaStore::init(dir.path()).unwrap()
    }

    #[test]
    fn quota_day_follows_pacific_not_utc() {
        // 07:30 UTC is still the previous day in California.
        assert_eq!(quota_day(at("2026-09-25T07:30:00Z")), "2026-09-25");
        assert_eq!(quota_day(at("2026-09-26T06:59:00Z")), "2026-09-25");
        assert_eq!(quota_day(at("2026-09-26T07:01:00Z")), "2026-09-26");
    }

    #[test]
    fn reset_is_reported_in_the_readers_timezone() {
        // Pacific midnight on 09-26 is 17:00 the same day in Sydney (AEST).
        assert_eq!(
            reset_label(at("2026-09-26T02:00:00Z"), "Australia/Sydney").as_deref(),
            Some("17:00")
        );
        assert!(reset_label(at("2026-09-26T02:00:00Z"), "Not/AZone").is_none());
    }

    #[test]
    fn counts_are_grouped_by_key_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let now = at("2026-09-26T02:00:00Z");
        for _ in 0..3 {
            store.record_call("gemini", "gemini-3.5-flash", now);
        }
        store.record_call("gemini#2", "gemini-3.5-flash", now);
        store.record_call("gemini", "gemini-3.6-flash", now);

        let usage = store.today(now);
        assert_eq!(usage[0].model, "gemini-3.5-flash", "busiest model first");
        assert_eq!((usage[0].calls, usage[0].keys_seen), (4, 2));
        assert_eq!(usage[1].calls, 1);
        assert!(usage.iter().all(|u| u.observed_limit.is_none()));
    }

    #[test]
    fn a_later_pacific_day_starts_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let today_utc = at("2026-09-26T02:00:00Z");
        store.record_call("gemini", "m", today_utc);
        // Still the same Pacific day a few hours later.
        store.record_call("gemini", "m", at("2026-09-26T06:00:00Z"));
        assert_eq!(store.today(today_utc)[0].calls, 2);
        // Past Pacific midnight the count restarts.
        assert!(store.today(at("2026-09-26T08:00:00Z")).is_empty());
    }

    #[test]
    fn running_out_records_the_observed_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        let now = at("2026-09-26T02:00:00Z");
        for _ in 0..5 {
            store.record_call("gemini", "m", now);
        }
        store.record_exhausted("gemini", "m", now);
        // A second key that got further raises the estimate.
        for _ in 0..9 {
            store.record_call("gemini#2", "m", now);
        }
        store.record_exhausted("gemini#2", "m", now);

        let u = &store.today(now)[0];
        assert_eq!(u.observed_limit, Some(9), "largest seen wins");
        assert_eq!((u.calls, u.keys_exhausted), (14, 2));
        // Two keys at 9 each = 18 budget, 14 used.
        assert_eq!(u.remaining(2), Some(4));
        assert_eq!(u.remaining(6), Some(40));
    }

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_known_ceiling_shows_used_total_and_how_the_total_was_derived() {
        let usage = vec![ModelUsage {
            model: "gemini-3.5-flash".into(),
            calls: 180,
            keys_seen: 6,
            keys_exhausted: 1,
            observed_limit: Some(100),
        }];
        let configured = models(&["gemini-3.5-flash", "gemini-3.8-flash"]);
        let out = render_summary(
            &usage,
            &configured,
            6,
            at("2026-09-26T02:00:00Z"),
            "Australia/Sydney",
        )
        .unwrap();
        assert!(out.contains("🔑 今日用量（17:00 重置）"), "{out}");
        assert!(
            out.contains("• 3.5-flash 180/约 600（6 个 key × 单 key 约 100），1 个 key 已用完"),
            "{out}"
        );
        // Models that exist in config but were not called are still accounted for.
        assert!(out.contains("• 今天没用到：3.8-flash"), "{out}");
        // The quota timezone is an implementation detail, never shown.
        assert!(!out.contains("太平洋") && !out.contains("Pacific"), "{out}");
    }

    #[test]
    fn an_unknown_ceiling_says_so_instead_of_showing_a_bare_number() {
        let usage = vec![
            ModelUsage {
                model: "gemini-3.6-flash".into(),
                calls: 2,
                keys_seen: 1,
                keys_exhausted: 0,
                observed_limit: None,
            },
            ModelUsage {
                model: "gemini-3.5-flash-lite".into(),
                calls: 87,
                keys_seen: 3,
                keys_exhausted: 0,
                observed_limit: None,
            },
        ];
        let out = render_summary(
            &usage,
            &models(&["gemini-3.6-flash", "gemini-3.5-flash-lite"]),
            6,
            at("2026-09-26T02:00:00Z"),
            "Australia/Sydney",
        )
        .unwrap();
        assert!(
            out.contains("• 3.6-flash 2、3.5-flash-lite 87（还没有 key 触顶，上限未知）"),
            "{out}"
        );
        assert!(!out.contains("今天没用到"), "{out}");
        assert!(!out.contains("已用完"), "{out}");
    }

    #[test]
    fn no_calls_today_means_no_note_at_all() {
        let configured = models(&["gemini-3.5-flash"]);
        assert!(render_summary(
            &[],
            &configured,
            6,
            at("2026-09-26T02:00:00Z"),
            "Australia/Sydney"
        )
        .is_none());
    }

    #[test]
    fn retired_lists_only_models_the_api_dropped() {
        let configured = vec![
            "gemini-3.5-flash".to_string(),
            "gemini-2.0-flash".to_string(),
            "gemini-3.6-flash".to_string(),
        ];
        let available: std::collections::HashSet<String> =
            ["gemini-3.5-flash", "gemini-3.6-flash", "gemini-3.8-flash"]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
        assert_eq!(retired(&configured, &available), vec!["gemini-2.0-flash"]);
        assert!(retired(&[], &available).is_empty());
    }

    #[test]
    fn the_model_check_runs_once_per_quota_day() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(&dir);
        assert_eq!(store.model_check("2026-09-26"), None, "not run yet");
        store.set_model_check("2026-09-26", "");
        assert_eq!(
            store.model_check("2026-09-26"),
            Some(String::new()),
            "ran, nothing missing"
        );
        assert_eq!(
            store.model_check("2026-09-27"),
            None,
            "a new day checks again"
        );
    }
}
