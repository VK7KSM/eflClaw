//! Shared runtime load tracker for team/subagent orchestration.
//!
//! The tracker records in-flight counts and recent assignment/failure events
//! per agent. Selection logic can then apply dynamic load-aware penalties
//! without hardcoding specific agent identities.
//!
//! Ported from upstream zeroclaw `tools/agent_load_tracker.rs`.

use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Minimum retention window for pruning event queues.
const MIN_RETENTION: Duration = Duration::from_secs(60);

/// Snapshot of a single agent's current load state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentLoadSnapshot {
    /// Number of tasks currently in flight for this agent.
    pub in_flight: usize,
    /// Number of assignment events within the query window.
    pub recent_assignments: usize,
    /// Number of failure events within the query window.
    pub recent_failures: usize,
}

#[derive(Debug, Default)]
struct AgentRuntimeLoad {
    in_flight: usize,
    assignment_events: VecDeque<Instant>,
    failure_events: VecDeque<Instant>,
}

/// Thread-safe runtime load tracker for all known agent names.
///
/// Used by agent selection logic to make load-aware routing decisions.
///
/// # Usage
///
/// ```ignore
/// let tracker = AgentLoadTracker::new();
/// let mut lease = tracker.start("coder");
/// // ... do work ...
/// lease.mark_success();
/// ```
#[derive(Clone, Default)]
pub struct AgentLoadTracker {
    inner: Arc<RwLock<HashMap<String, AgentRuntimeLoad>>>,
}

impl AgentLoadTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark an assignment as started and return a lease that must be finalized.
    ///
    /// The lease decrements `in_flight` and records the outcome when dropped or
    /// when `mark_success()` / `mark_failure()` is called explicitly.
    pub fn start(&self, agent_name: &str) -> AgentLoadLease {
        let agent = agent_name.trim();
        if agent.is_empty() {
            return AgentLoadLease::noop(self.clone());
        }

        let now = Instant::now();
        let mut map = self.inner.write();
        let state = map.entry(agent.to_string()).or_default();
        state.in_flight = state.in_flight.saturating_add(1);
        state.assignment_events.push_back(now);
        Self::prune_state(state, now, Duration::from_secs(600));

        AgentLoadLease {
            tracker: self.clone(),
            agent_name: agent.to_string(),
            finalized: false,
            active: true,
        }
    }

    /// Record a direct failure (for example provider creation failure)
    /// without an associated `start()` call.
    pub fn record_failure(&self, agent_name: &str) {
        let agent = agent_name.trim();
        if agent.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut map = self.inner.write();
        let state = map.entry(agent.to_string()).or_default();
        state.failure_events.push_back(now);
        Self::prune_state(state, now, Duration::from_secs(600));
    }

    /// Return current load snapshots using the provided recent-event window.
    ///
    /// Events older than `window` are excluded from the counts but retained in
    /// memory until the retention period (4× window, min 60 s) expires.
    pub fn snapshot(&self, window: Duration) -> HashMap<String, AgentLoadSnapshot> {
        let effective_window = if window.is_zero() {
            Duration::from_secs(1)
        } else {
            window
        };
        let retention = effective_window.checked_mul(4).unwrap_or(effective_window);
        let retention = retention.max(MIN_RETENTION);
        let now = Instant::now();

        let mut map = self.inner.write();
        let mut out = HashMap::new();
        for (agent, state) in map.iter_mut() {
            Self::prune_state(state, now, retention);
            let recent_assignments = state
                .assignment_events
                .iter()
                .filter(|ts| now.saturating_duration_since(**ts) <= effective_window)
                .count();
            let recent_failures = state
                .failure_events
                .iter()
                .filter(|ts| now.saturating_duration_since(**ts) <= effective_window)
                .count();
            out.insert(
                agent.clone(),
                AgentLoadSnapshot {
                    in_flight: state.in_flight,
                    recent_assignments,
                    recent_failures,
                },
            );
        }
        out
    }

    fn finish(&self, agent_name: &str, success: bool) {
        let agent = agent_name.trim();
        if agent.is_empty() {
            return;
        }

        let now = Instant::now();
        let mut map = self.inner.write();
        let state = map.entry(agent.to_string()).or_default();
        state.in_flight = state.in_flight.saturating_sub(1);
        if !success {
            state.failure_events.push_back(now);
        }
        Self::prune_state(state, now, Duration::from_secs(600));
    }

    fn prune_state(state: &mut AgentRuntimeLoad, now: Instant, retention: Duration) {
        while state
            .assignment_events
            .front()
            .is_some_and(|ts| now.saturating_duration_since(*ts) > retention)
        {
            state.assignment_events.pop_front();
        }
        while state
            .failure_events
            .front()
            .is_some_and(|ts| now.saturating_duration_since(*ts) > retention)
        {
            state.failure_events.pop_front();
        }
    }
}

/// RAII lease returned by [`AgentLoadTracker::start`].
///
/// Must be finalized via [`mark_success`](Self::mark_success) or
/// [`mark_failure`](Self::mark_failure). If dropped without explicit
/// finalization, the outcome is recorded as a **failure** to ensure
/// `in_flight` never leaks.
pub struct AgentLoadLease {
    tracker: AgentLoadTracker,
    agent_name: String,
    finalized: bool,
    active: bool,
}

impl AgentLoadLease {
    fn noop(tracker: AgentLoadTracker) -> Self {
        Self {
            tracker,
            agent_name: String::new(),
            finalized: true,
            active: false,
        }
    }

    /// Finalize as success — decrements `in_flight`, no failure recorded.
    pub fn mark_success(&mut self) {
        if !self.active || self.finalized {
            return;
        }
        self.tracker.finish(&self.agent_name, true);
        self.finalized = true;
    }

    /// Finalize as failure — decrements `in_flight`, records one failure event.
    pub fn mark_failure(&mut self) {
        if !self.active || self.finalized {
            return;
        }
        self.tracker.finish(&self.agent_name, false);
        self.finalized = true;
    }
}

impl Drop for AgentLoadLease {
    fn drop(&mut self) {
        if !self.active || self.finalized {
            return;
        }
        // Safety net: unfinalized lease → record as failure
        self.tracker.finish(&self.agent_name, false);
        self.finalized = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_inflight_and_completion() {
        let tracker = AgentLoadTracker::new();
        let mut lease = tracker.start("coder");

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("coder").map(|e| e.in_flight), Some(1));
        assert_eq!(snap.get("coder").map(|e| e.recent_assignments), Some(1));

        lease.mark_success();

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("coder").map(|e| e.in_flight), Some(0));
        assert_eq!(snap.get("coder").map(|e| e.recent_failures), Some(0));
    }

    #[test]
    fn dropped_lease_marks_failure_and_releases_inflight() {
        let tracker = AgentLoadTracker::new();
        {
            let _lease = tracker.start("researcher");
        }

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("researcher").map(|e| e.in_flight), Some(0));
        assert_eq!(snap.get("researcher").map(|e| e.recent_failures), Some(1));
    }

    #[test]
    fn record_failure_without_start_is_counted() {
        let tracker = AgentLoadTracker::new();
        tracker.record_failure("planner");

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("planner").map(|e| e.in_flight), Some(0));
        assert_eq!(snap.get("planner").map(|e| e.recent_failures), Some(1));
    }

    #[test]
    fn empty_agent_name_is_silently_ignored() {
        let tracker = AgentLoadTracker::new();
        let mut lease = tracker.start("  ");
        lease.mark_success();
        let snap = tracker.snapshot(Duration::from_secs(60));
        assert!(snap.is_empty());
    }

    #[test]
    fn multiple_concurrent_leases_accumulate_inflight() {
        let tracker = AgentLoadTracker::new();
        let _lease1 = tracker.start("worker");
        let _lease2 = tracker.start("worker");
        let lease3 = tracker.start("worker");

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("worker").map(|e| e.in_flight), Some(3));

        drop(lease3);

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("worker").map(|e| e.in_flight), Some(2));
    }

    #[test]
    fn mark_success_is_idempotent() {
        let tracker = AgentLoadTracker::new();
        let mut lease = tracker.start("agent");
        lease.mark_success();
        lease.mark_success(); // second call should be a no-op

        let snap = tracker.snapshot(Duration::from_secs(60));
        assert_eq!(snap.get("agent").map(|e| e.in_flight), Some(0));
        assert_eq!(snap.get("agent").map(|e| e.recent_failures), Some(0));
    }
}
