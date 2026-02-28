//! Provider health tracker — records failure counts and circuit-breaker state.
//!
//! Used by the quota CLI to show which providers are degraded or fully offline.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Snapshot of a single provider's health at query time.
#[derive(Debug, Clone)]
pub struct HealthState {
    /// Number of consecutive failures (reset on success)
    pub failure_count: u32,
    /// Last error message recorded
    pub last_error: Option<String>,
    /// Whether the circuit breaker is currently open
    pub circuit_open: bool,
}

/// Inner state for a tracked provider.
#[derive(Debug)]
struct ProviderEntry {
    failure_count: u32,
    last_error: Option<String>,
    last_failure_at: Option<Instant>,
}

/// Thread-safe provider health tracker.
///
/// Records consecutive failure counts and implements a simple circuit-breaker:
/// once `failure_threshold` consecutive failures occur, the circuit opens until
/// the `cooldown` duration elapses since the last failure.
pub struct ProviderHealthTracker {
    failure_threshold: u32,
    cooldown: Duration,
    entries: Mutex<HashMap<String, ProviderEntry>>,
}

impl ProviderHealthTracker {
    /// Create a new tracker.
    ///
    /// - `failure_threshold`: failures before circuit opens
    /// - `cooldown`: how long circuit stays open after last failure
    /// - `_max_tracked`: maximum number of providers to track (unused, for API compat)
    pub fn new(failure_threshold: u32, cooldown: Duration, _max_tracked: usize) -> Self {
        Self {
            failure_threshold,
            cooldown,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Record a successful call for a provider (resets failure count).
    pub fn record_success(&self, provider: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.get_mut(provider) {
            entry.failure_count = 0;
            entry.last_error = None;
            entry.last_failure_at = None;
        }
    }

    /// Record a failed call for a provider.
    pub fn record_failure(&self, provider: &str, error: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let entry = entries.entry(provider.to_string()).or_insert(ProviderEntry {
            failure_count: 0,
            last_error: None,
            last_failure_at: None,
        });
        entry.failure_count = entry.failure_count.saturating_add(1);
        entry.last_error = Some(error.to_string());
        entry.last_failure_at = Some(Instant::now());
    }

    /// Check whether the circuit for a provider is currently open.
    pub fn is_circuit_open(&self, provider: &str) -> bool {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get(provider) else {
            return false;
        };
        if entry.failure_count < self.failure_threshold {
            return false;
        }
        // Circuit stays open until cooldown elapses since last failure
        entry
            .last_failure_at
            .map(|t| t.elapsed() < self.cooldown)
            .unwrap_or(false)
    }

    /// Retrieve health state snapshots for all tracked providers.
    pub fn get_all_states(&self) -> HashMap<String, HealthState> {
        let entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries
            .iter()
            .map(|(name, entry)| {
                let circuit_open = entry.failure_count >= self.failure_threshold
                    && entry
                        .last_failure_at
                        .map(|t| t.elapsed() < self.cooldown)
                        .unwrap_or(false);
                (
                    name.clone(),
                    HealthState {
                        failure_count: entry.failure_count,
                        last_error: entry.last_error.clone(),
                        circuit_open,
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_tracker_records_failures() {
        let tracker = ProviderHealthTracker::new(3, Duration::from_secs(60), 100);
        tracker.record_failure("openai", "timeout");
        tracker.record_failure("openai", "timeout");

        let states = tracker.get_all_states();
        let state = states.get("openai").unwrap();
        assert_eq!(state.failure_count, 2);
        assert!(!state.circuit_open, "circuit should not open below threshold");
    }

    #[test]
    fn health_tracker_circuit_opens_at_threshold() {
        let tracker = ProviderHealthTracker::new(3, Duration::from_secs(60), 100);
        tracker.record_failure("anthropic", "rate_limit");
        tracker.record_failure("anthropic", "rate_limit");
        tracker.record_failure("anthropic", "rate_limit");

        let states = tracker.get_all_states();
        let state = states.get("anthropic").unwrap();
        assert_eq!(state.failure_count, 3);
        assert!(state.circuit_open);
    }

    #[test]
    fn health_tracker_success_resets_failure_count() {
        let tracker = ProviderHealthTracker::new(3, Duration::from_secs(60), 100);
        tracker.record_failure("gemini", "error");
        tracker.record_failure("gemini", "error");
        tracker.record_success("gemini");

        let states = tracker.get_all_states();
        let state = states.get("gemini").unwrap();
        assert_eq!(state.failure_count, 0);
        assert!(!state.circuit_open);
    }

    #[test]
    fn health_tracker_empty_provider_not_circuit_open() {
        let tracker = ProviderHealthTracker::new(3, Duration::from_secs(60), 100);
        assert!(!tracker.is_circuit_open("unknown"));
    }
}
