//! Time-based score decay for memory entries.
//!
//! Applies exponential half-life decay to non-Core memories so that older
//! entries naturally rank lower in relevance searches without being deleted.
//!
//! Ported from upstream zeroclaw `memory/decay.rs`.

use crate::memory::traits::{MemoryCategory, MemoryEntry};
use chrono::{DateTime, Utc};

/// Default half-life in days: a memory's score halves every 7 days.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 7.0;

/// Apply exponential time decay to memory entry scores in-place.
///
/// Formula: `new_score = old_score * 2^(-age_days / half_life_days)`
///
/// - `MemoryCategory::Core` entries are exempt — they never decay.
/// - Entries without a valid RFC3339 timestamp are left unchanged.
/// - Entries without a score (`score == None`) are left unchanged.
pub fn apply_time_decay(
    memories: &mut [MemoryEntry],
    reference_time: DateTime<Utc>,
    half_life_days: f64,
) {
    let effective_half_life = if half_life_days <= 0.0 {
        DEFAULT_HALF_LIFE_DAYS
    } else {
        half_life_days
    };

    for memory in memories.iter_mut() {
        // Core memories are evergreen — never decay
        if memory.category == MemoryCategory::Core {
            continue;
        }

        // Skip entries without a score
        let Some(score) = memory.score else {
            continue;
        };

        // Parse the timestamp; leave unchanged if unparseable
        let Ok(created_at) = memory.timestamp.parse::<DateTime<Utc>>() else {
            continue;
        };

        let age_secs = reference_time
            .signed_duration_since(created_at)
            .num_seconds();
        // Clamp negative ages (future timestamps) to 0 — no boost for future entries
        let age_days = (age_secs as f64 / 86400.0).max(0.0);

        let decay_factor = 2.0_f64.powf(-age_days / effective_half_life);
        memory.score = Some(score * decay_factor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::traits::MemoryCategory;
    use chrono::Duration;

    fn make_entry(category: MemoryCategory, timestamp: &str, score: Option<f64>) -> MemoryEntry {
        MemoryEntry {
            id: "test-id".into(),
            key: "test-key".into(),
            content: "test content".into(),
            category,
            timestamp: timestamp.into(),
            session_id: None,
            score,
        }
    }

    #[test]
    fn core_memory_is_exempt_from_decay() {
        let now = Utc::now();
        let old_ts = (now - Duration::days(30)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Core, &old_ts, Some(1.0))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        assert!(
            (entries[0].score.unwrap() - 1.0).abs() < 1e-9,
            "Core memory score must not change"
        );
    }

    #[test]
    fn recent_memory_barely_changes() {
        let now = Utc::now();
        let ts = (now - Duration::hours(1)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Daily, &ts, Some(1.0))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        let score = entries[0].score.unwrap();
        // 1 hour out of 168 hours (7 days) — decay is tiny
        assert!(score > 0.99, "1-hour-old memory should barely decay: {score}");
    }

    #[test]
    fn one_half_life_halves_the_score() {
        let now = Utc::now();
        let ts = (now - Duration::days(7)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Daily, &ts, Some(1.0))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        let score = entries[0].score.unwrap();
        assert!(
            (score - 0.5).abs() < 1e-6,
            "After 7 days (1 half-life), score should be ~0.5, got {score}"
        );
    }

    #[test]
    fn two_half_lives_quarters_the_score() {
        let now = Utc::now();
        let ts = (now - Duration::days(14)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Conversation, &ts, Some(1.0))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        let score = entries[0].score.unwrap();
        assert!(
            (score - 0.25).abs() < 1e-6,
            "After 14 days (2 half-lives), score should be ~0.25, got {score}"
        );
    }

    #[test]
    fn no_score_entry_is_unchanged() {
        let now = Utc::now();
        let ts = (now - Duration::days(30)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Daily, &ts, None)];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        assert!(entries[0].score.is_none(), "None score should remain None");
    }

    #[test]
    fn unparseable_timestamp_is_unchanged() {
        let now = Utc::now();
        let mut entries = vec![make_entry(MemoryCategory::Daily, "not-a-date", Some(0.8))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        assert!(
            (entries[0].score.unwrap() - 0.8).abs() < 1e-9,
            "Unparseable timestamp should leave score unchanged"
        );
    }

    #[test]
    fn future_timestamp_does_not_boost_score() {
        let now = Utc::now();
        let future_ts = (now + Duration::days(7)).to_rfc3339();
        let mut entries = vec![make_entry(MemoryCategory::Daily, &future_ts, Some(0.5))];

        apply_time_decay(&mut entries, now, DEFAULT_HALF_LIFE_DAYS);

        // age clamped to 0 → decay_factor = 1.0 → score unchanged
        let score = entries[0].score.unwrap();
        assert!(
            (score - 0.5).abs() < 1e-9,
            "Future timestamp should not boost score: {score}"
        );
    }
}
