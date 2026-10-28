//! Gemini calls cctv-ai makes on elfClaw's keys, counted as elfClaw's own.
//!
//! The owner decided (2026-10-20) that cctv shares elfClaw's six official
//! Gemini keys — K5 user environment variable `GEMINI_API_KEYS`, same order
//! as elfClaw's labels `gemini`, `gemini#2` … `gemini#6` — and that its calls
//! count against elfClaw's usage. cctv-ai logs every online-AI call with the
//! key's label in `provider`; this task reads them back from
//! `GET /cctv/v1/ai/usage` (v1.15.2) every few minutes and adds the Gemini
//! ones (and its Antigravity proxy calls) to `state/quota.db`, so the daily figures and the note under every
//! push include cctv. A `quota_day:` error also marks that key spent for the
//! day, which makes the observed per-key limit truer, not lower.

use super::api::{CctvApi, UsageRecord};
use super::store::Store;
use chrono::{DateTime, TimeZone, Utc};
use std::path::PathBuf;
use std::time::Duration;

/// How often cctv's log is read.
const EVERY: Duration = Duration::from_secs(300);
/// Records per request (cctv's maximum).
const PAGE: usize = 500;
/// cctv.db kv key holding the last record id taken in.
const CURSOR: &str = "ai_usage_after";
/// Older calls belong to quota days that are over; they are skipped.
const MAX_AGE: chrono::Duration = chrono::Duration::days(2);

/// What one record adds to the accounting.
#[derive(Debug, PartialEq)]
enum Counted<'a> {
    Call(&'a str, &'a str, DateTime<Utc>),
    Exhausted(&'a str, &'a str, DateTime<Utc>),
}

/// elfgate's Gemini names carry the thinking level (`-low/-medium/-high`);
/// elfClaw records the chain's name.
fn chain_model(model: &str) -> &str {
    if !model.starts_with("gemini-") {
        return model;
    }
    ["-low", "-medium", "-high"]
        .iter()
        .find_map(|s| model.strip_suffix(s))
        .unwrap_or(model)
}

fn counted(r: &UsageRecord, now: DateTime<Utc>) -> Option<Counted<'_>> {
    let at = Utc.timestamp_millis_opt(r.t_ms).single()?;
    if now - at > MAX_AGE {
        return None;
    }
    // 2026-10-28: cctv-ai calls elfgate first (`elfgate`; until then the
    // Antigravity proxy); its calls join elfClaw's own elfgate count, under
    // the chain's model name (cctv asks for `gemini-3.8-flash-medium`).
    // No Google daily quota of ours, so only successes count there.
    if r.provider == "elfgate" {
        return r.ok.then(|| {
            Counted::Call(
                crate::providers::reliable::ELFGATE_PROVIDER,
                chain_model(&r.model),
                at,
            )
        });
    }
    let label = r.provider.as_str();
    if label != "gemini" && !label.starts_with("gemini#") {
        return None;
    }
    if r.ok {
        return Some(Counted::Call(label, &r.model, at));
    }
    r.error
        .as_deref()
        .is_some_and(|e| e.starts_with("quota_day:"))
        .then(|| Counted::Exhausted(label, &r.model, at))
}

pub async fn run(api: CctvApi, workspace: PathBuf) {
    let store = match Store::open(&workspace) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("CCTV usage: cannot open store: {e:#}");
            return;
        }
    };
    loop {
        if let Err(e) = pull(&api, &store).await {
            // Until cctv ships v1.15.2 this is a 404 every few minutes.
            tracing::debug!("CCTV usage: {e:#}");
        }
        tokio::time::sleep(EVERY).await;
    }
}

async fn pull(api: &CctvApi, store: &Store) -> anyhow::Result<()> {
    let mut after: i64 = store.get(CURSOR)?.and_then(|c| c.parse().ok()).unwrap_or(0);
    loop {
        let page = api.ai_usage(after, PAGE).await?;
        let now = Utc::now();
        for r in &page.records {
            match counted(r, now) {
                Some(Counted::Call(label, model, at)) => {
                    crate::providers::quota::record_call_at(label, model, at);
                }
                Some(Counted::Exhausted(label, model, at)) => {
                    crate::providers::quota::record_exhausted_at(label, model, at);
                }
                None => {}
            }
        }
        if page.next_after_id <= after {
            return Ok(());
        }
        after = page.next_after_id;
        store.set(CURSOR, &after.to_string())?;
        if page.records.len() < PAGE {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(provider: &str, ok: bool, error: Option<&str>, t_ms: i64) -> UsageRecord {
        UsageRecord {
            id: 1,
            t_ms,
            provider: provider.into(),
            model: "gemini-3.8-flash".into(),
            ok,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn gemini_calls_count_and_others_do_not() {
        let now = Utc::now();
        let t = now.timestamp_millis();
        let at = Utc.timestamp_millis_opt(t).unwrap();
        assert_eq!(
            counted(&rec("gemini#2", true, None, t), now),
            Some(Counted::Call("gemini#2", "gemini-3.8-flash", at))
        );
        assert_eq!(
            counted(&rec("gemini", true, None, t), now),
            Some(Counted::Call("gemini", "gemini-3.8-flash", at))
        );
        assert_eq!(counted(&rec("anthropic", true, None, t), now), None);
        assert_eq!(counted(&rec("gemini_proxy", true, None, t), now), None);
        // elfgate: successes only, under elfClaw's elfgate label and the
        // chain's model name.
        let medium = UsageRecord {
            model: "gemini-3.8-flash-medium".into(),
            ..rec("elfgate", true, None, t)
        };
        assert_eq!(
            counted(&medium, now),
            Some(Counted::Call("elfgate", "gemini-3.8-flash", at))
        );
        let gpt = UsageRecord {
            model: "gpt-oss-120b-medium".into(),
            ..rec("elfgate", true, None, t)
        };
        assert_eq!(
            counted(&gpt, now),
            Some(Counted::Call("elfgate", "gpt-oss-120b-medium", at))
        );
        assert_eq!(
            counted(&rec("elfgate", false, Some("quota_low"), t), now),
            None
        );
        assert_eq!(counted(&rec("antigravity", true, None, t), now), None);
    }

    #[test]
    fn only_daily_quota_errors_mark_a_key_spent() {
        let now = Utc::now();
        let t = now.timestamp_millis();
        let at = Utc.timestamp_millis_opt(t).unwrap();
        assert_eq!(
            counted(&rec("gemini#3", false, Some("quota_day: 429 ..."), t), now),
            Some(Counted::Exhausted("gemini#3", "gemini-3.8-flash", at))
        );
        for e in ["quota_minute: 429", "http_500: x", "net: x", "parse: x"] {
            assert_eq!(counted(&rec("gemini#3", false, Some(e), t), now), None);
        }
    }

    #[test]
    fn calls_from_past_quota_days_are_skipped() {
        let now = Utc::now();
        let old = (now - chrono::Duration::days(3)).timestamp_millis();
        assert_eq!(counted(&rec("gemini", true, None, old), now), None);
    }
}
