//! Loop detection for the agent tool-call loop.
//!
//! Detects three patterns of unproductive looping:
//! 1. **No-progress repeat** — same tool + same args + same output hash.
//! 2. **Ping-pong** — two calls alternating (A→B→A→B) with no progress.
//! 3. **Consecutive failure streak** — same tool failing repeatedly.
//!
//! On first detection an `InjectWarning` verdict gives the LLM a chance to
//! self-correct.  If the pattern persists the next check returns `HardStop`.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};

// elfClaw: action tools that fire real-world effects; monitored for per-turn spam
const ACTION_SPAM_TOOLS: &[&str] = &["send_voice", "send_email", "send_telegram"];

/// Maximum bytes of tool output considered when hashing results.
/// Keeps hashing fast and bounded for large outputs.
const OUTPUT_HASH_PREFIX_BYTES: usize = 4096;

// ─── Configuration ───────────────────────────────────────────────────────────

/// Tuning knobs for each detection strategy.
#[derive(Debug, Clone)]
pub(crate) struct LoopDetectionConfig {
    /// Identical (tool + args + output) repetitions before triggering.
    /// `0` = disabled.  Default: `3`.
    pub no_progress_threshold: usize,
    /// Full A-B cycles before triggering ping-pong detection.
    /// `0` = disabled.  Default: `2`.
    pub ping_pong_cycles: usize,
    /// Consecutive failures of the *same* tool before triggering.
    /// `0` = disabled.  Default: `3`.
    pub failure_streak_threshold: usize,
    /// elfClaw: max successful invocations of an action tool before triggering spam watchdog.
    /// At this count the detector injects a warning; above it issues a HardStop.
    /// `0` = disabled.  Default: `1` (warn after 1st, stop after 2nd).
    pub action_success_limit: usize,
}

impl Default for LoopDetectionConfig {
    fn default() -> Self {
        Self {
            no_progress_threshold: 3,
            ping_pong_cycles: 2,
            failure_streak_threshold: 3,
            action_success_limit: 1,
        }
    }
}

// ─── Verdict ─────────────────────────────────────────────────────────────────

/// Action the caller should take after `LoopDetector::check()`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DetectionVerdict {
    /// No loop detected — proceed normally.
    Continue,
    /// First detection — inject this self-correction prompt, then continue.
    InjectWarning(String),
    /// Pattern persisted after warning — terminate the loop.
    HardStop(String),
}

// ─── Internal record ─────────────────────────────────────────────────────────

struct CallRecord {
    tool_name: String,
    args_sig: String,
    result_hash: u64,
    success: bool,
}

// ─── Detector ────────────────────────────────────────────────────────────────

pub(crate) struct LoopDetector {
    config: LoopDetectionConfig,
    history: Vec<CallRecord>,
    consecutive_failures: HashMap<String, usize>,
    warning_injected: bool,
    /// elfClaw: per-tool success counts for action spam detection (only ACTION_SPAM_TOOLS tracked)
    success_counts: HashMap<String, usize>,
    /// elfClaw: tracks which action tools have already received a spam warning this turn
    success_spam_warned: HashSet<String>,
    /// elfClaw: last failed args per tool (for error learning reports)
    last_failed_args: HashMap<String, String>,
}

impl LoopDetector {
    pub fn new(config: LoopDetectionConfig) -> Self {
        Self {
            config,
            history: Vec::new(),
            consecutive_failures: HashMap::new(),
            warning_injected: false,
            success_counts: HashMap::new(),
            success_spam_warned: HashSet::new(),
            last_failed_args: HashMap::new(),
        }
    }

    /// Record a completed tool invocation.
    ///
    /// * `tool_name` — canonical tool name (lowercased by caller).
    /// * `args_sig`  — canonical JSON args string from `tool_call_signature()`.
    /// * `output`    — raw tool output text.
    /// * `success`   — whether the tool reported success.
    pub fn record_call(&mut self, tool_name: &str, args_sig: &str, output: &str, success: bool) {
        let result_hash = hash_output(output);
        self.history.push(CallRecord {
            tool_name: tool_name.to_owned(),
            args_sig: args_sig.to_owned(),
            result_hash,
            success,
        });

        if success {
            self.consecutive_failures.remove(tool_name);
            // elfClaw: Fix 1 — track action tool successes for spam watchdog
            if ACTION_SPAM_TOOLS.contains(&tool_name) {
                *self.success_counts.entry(tool_name.to_owned()).or_insert(0) += 1;
            }
        } else {
            *self
                .consecutive_failures
                .entry(tool_name.to_owned())
                .or_insert(0) += 1;
            // elfClaw: track last failed args for error learning reports
            self.last_failed_args
                .insert(tool_name.to_string(), args_sig.chars().take(200).collect());
        }
    }

    /// Evaluate the current history and return a verdict.
    pub fn check(&mut self) -> DetectionVerdict {
        let reason = self
            .check_no_progress_repeat()
            .or_else(|| self.check_ping_pong())
            .or_else(|| self.check_failure_streak());

        let existing_verdict = match reason {
            None => DetectionVerdict::Continue,
            Some(msg) => {
                if self.warning_injected {
                    DetectionVerdict::HardStop(msg)
                } else {
                    self.warning_injected = true;
                    DetectionVerdict::InjectWarning(format_warning(&msg))
                }
            }
        };

        if existing_verdict != DetectionVerdict::Continue {
            return existing_verdict;
        }

        // elfClaw: Fix 1 — strategy 4: action spam watchdog
        // Fires independently of the existing warning_injected flag.
        if let Some(verdict) = self.check_action_success_spam() {
            return verdict;
        }

        DetectionVerdict::Continue
    }

    /// Get a summary of all consecutive failure streaks for error reporting.
    pub fn failure_summary(&self) -> Vec<(String, usize)> {
        self.consecutive_failures
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(tool, count)| (tool.clone(), *count))
            .collect()
    }

    /// Last failed arguments per tool (truncated to 200 chars).
    pub fn last_failed_args(&self) -> &HashMap<String, String> {
        &self.last_failed_args
    }

    // ── Strategy 1: no-progress repeat ───────────────────────────────────

    fn check_no_progress_repeat(&self) -> Option<String> {
        let threshold = self.config.no_progress_threshold;
        if threshold == 0 || self.history.is_empty() {
            return None;
        }
        let last = self.history.last().unwrap();
        let streak = self
            .history
            .iter()
            .rev()
            .take_while(|r| {
                r.tool_name == last.tool_name
                    && r.args_sig == last.args_sig
                    && r.result_hash == last.result_hash
            })
            .count();
        if streak >= threshold {
            Some(format!(
                "Tool '{}' called {} times with identical arguments and identical results \
                 — no progress detected",
                last.tool_name, streak
            ))
        } else {
            None
        }
    }

    // ── Strategy 2: ping-pong ────────────────────────────────────────────

    fn check_ping_pong(&self) -> Option<String> {
        let cycles = self.config.ping_pong_cycles;
        if cycles == 0 || self.history.len() < 4 {
            return None;
        }
        let len = self.history.len();
        let a = &self.history[len - 2];
        let b = &self.history[len - 1];

        // The two sides of the ping-pong must differ.
        if a.tool_name == b.tool_name && a.args_sig == b.args_sig {
            return None;
        }

        let min_entries = cycles * 2;
        if len < min_entries {
            return None;
        }
        let tail = &self.history[len - min_entries..];
        let is_ping_pong = tail.chunks(2).all(|pair| {
            pair.len() == 2
                && pair[0].tool_name == a.tool_name
                && pair[0].args_sig == a.args_sig
                && pair[0].result_hash == a.result_hash
                && pair[1].tool_name == b.tool_name
                && pair[1].args_sig == b.args_sig
                && pair[1].result_hash == b.result_hash
        });

        if is_ping_pong {
            Some(format!(
                "Ping-pong loop detected: '{}' and '{}' alternating {} times with no progress",
                a.tool_name, b.tool_name, cycles
            ))
        } else {
            None
        }
    }

    // ── Strategy 3: consecutive failure streak ───────────────────────────

    fn check_failure_streak(&self) -> Option<String> {
        let threshold = self.config.failure_streak_threshold;
        if threshold == 0 {
            return None;
        }
        for (tool, count) in &self.consecutive_failures {
            if *count >= threshold {
                return Some(format!(
                    "Tool '{}' failed {} consecutive times",
                    tool, count
                ));
            }
        }
        None
    }

    // ── elfClaw: Strategy 4: action spam watchdog ─────────────────────────

    /// Check whether any action tool has been called successfully more times
    /// than `action_success_limit` allows.  Returns an `InjectWarning` on first
    /// breach and a `HardStop` thereafter, using per-tool warned state so this
    /// strategy does not interfere with the shared `warning_injected` flag.
    fn check_action_success_spam(&mut self) -> Option<DetectionVerdict> {
        let limit = self.config.action_success_limit;
        if limit == 0 {
            return None;
        }
        // Snapshot to avoid borrow conflict when inserting into success_spam_warned.
        let candidates: Vec<(String, usize)> = self
            .success_counts
            .iter()
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        for (tool, count) in candidates {
            if count > limit {
                return Some(DetectionVerdict::HardStop(format!(
                    "Watchdog: tool `{tool}` succeeded {count} times in this turn. \
                     This action is complete — stop and give a final text response."
                )));
            } else if count == limit && !self.success_spam_warned.contains(&tool) {
                self.success_spam_warned.insert(tool.clone());
                return Some(DetectionVerdict::InjectWarning(format!(
                    "Watchdog: tool `{tool}` has already been called successfully. \
                     Do NOT call it again. Provide your final text response now."
                )));
            }
        }
        None
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn hash_output(output: &str) -> u64 {
    let prefix = if output.len() > OUTPUT_HASH_PREFIX_BYTES {
        // Use floor_utf8_char_boundary to avoid panic on multi-byte UTF-8 characters
        let boundary = crate::util::floor_utf8_char_boundary(output, OUTPUT_HASH_PREFIX_BYTES);
        &output[..boundary]
    } else {
        output
    };
    let mut hasher = DefaultHasher::new();
    prefix.hash(&mut hasher);
    hasher.finish()
}

fn format_warning(reason: &str) -> String {
    format!(
        "IMPORTANT: A loop pattern has been detected in your tool usage. {reason}. \
         You must change your approach: \
         (1) Try a different tool or different arguments, \
         (2) If polling a process, increase wait time or check if it's stuck, \
         (3) If the task cannot be completed, explain why and stop. \
         Do NOT repeat the same tool call with the same arguments."
    )
}

// ─── Unit tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> LoopDetectionConfig {
        LoopDetectionConfig::default()
    }

    fn disabled_config() -> LoopDetectionConfig {
        LoopDetectionConfig {
            no_progress_threshold: 0,
            ping_pong_cycles: 0,
            failure_streak_threshold: 0,
            action_success_limit: 0,
        }
    }

    // 1. Below threshold → Continue
    #[test]
    fn below_threshold_does_not_trigger() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("echo", r#"{"msg":"hi"}"#, "hello", true);
        det.record_call("echo", r#"{"msg":"hi"}"#, "hello", true);
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 2. No-progress repeat triggers warning at threshold
    #[test]
    fn no_progress_repeat_triggers_warning() {
        let mut det = LoopDetector::new(default_config());
        for _ in 0..3 {
            det.record_call("echo", r#"{"msg":"hi"}"#, "hello", true);
        }
        match det.check() {
            DetectionVerdict::InjectWarning(msg) => {
                assert!(msg.contains("no progress"), "msg: {msg}");
            }
            other => panic!("expected InjectWarning, got {other:?}"),
        }
    }

    // 3. Same input but different output → no trigger (progress detected)
    #[test]
    fn same_input_different_output_does_not_trigger() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("echo", r#"{"msg":"hi"}"#, "result_1", true);
        det.record_call("echo", r#"{"msg":"hi"}"#, "result_2", true);
        det.record_call("echo", r#"{"msg":"hi"}"#, "result_3", true);
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 4. Warning then continued loop → HardStop
    #[test]
    fn warning_then_continued_loop_triggers_hard_stop() {
        let mut det = LoopDetector::new(default_config());
        for _ in 0..3 {
            det.record_call("echo", r#"{"msg":"hi"}"#, "same", true);
        }
        assert!(matches!(det.check(), DetectionVerdict::InjectWarning(_)));
        // One more identical call
        det.record_call("echo", r#"{"msg":"hi"}"#, "same", true);
        match det.check() {
            DetectionVerdict::HardStop(msg) => {
                assert!(msg.contains("no progress"), "msg: {msg}");
            }
            other => panic!("expected HardStop, got {other:?}"),
        }
    }

    // 5. Ping-pong detection
    #[test]
    fn ping_pong_triggers_warning() {
        let mut det = LoopDetector::new(default_config());
        // 2 cycles: A-B-A-B
        det.record_call("tool_a", r#"{"x":1}"#, "out_a", true);
        det.record_call("tool_b", r#"{"y":2}"#, "out_b", true);
        det.record_call("tool_a", r#"{"x":1}"#, "out_a", true);
        det.record_call("tool_b", r#"{"y":2}"#, "out_b", true);
        match det.check() {
            DetectionVerdict::InjectWarning(msg) => {
                assert!(msg.contains("Ping-pong"), "msg: {msg}");
            }
            other => panic!("expected InjectWarning, got {other:?}"),
        }
    }

    // 6. Ping-pong with progress does not trigger
    #[test]
    fn ping_pong_with_progress_does_not_trigger() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("tool_a", r#"{"x":1}"#, "out_a_1", true);
        det.record_call("tool_b", r#"{"y":2}"#, "out_b_1", true);
        det.record_call("tool_a", r#"{"x":1}"#, "out_a_2", true); // different output
        det.record_call("tool_b", r#"{"y":2}"#, "out_b_2", true); // different output
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 7. Consecutive failure streak (different args each time to avoid no-progress trigger)
    #[test]
    fn failure_streak_triggers_warning() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("shell", r#"{"cmd":"bad1"}"#, "error: not found 1", false);
        det.record_call("shell", r#"{"cmd":"bad2"}"#, "error: not found 2", false);
        det.record_call("shell", r#"{"cmd":"bad3"}"#, "error: not found 3", false);
        match det.check() {
            DetectionVerdict::InjectWarning(msg) => {
                assert!(msg.contains("failed 3 consecutive"), "msg: {msg}");
            }
            other => panic!("expected InjectWarning, got {other:?}"),
        }
    }

    // 8. Failure streak resets on success
    #[test]
    fn failure_streak_resets_on_success() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("shell", r#"{"cmd":"bad"}"#, "err", false);
        det.record_call("shell", r#"{"cmd":"bad"}"#, "err", false);
        det.record_call("shell", r#"{"cmd":"good"}"#, "ok", true); // resets
        det.record_call("shell", r#"{"cmd":"bad"}"#, "err", false);
        det.record_call("shell", r#"{"cmd":"bad"}"#, "err", false);
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 9. All thresholds zero → disabled
    #[test]
    fn all_disabled_never_triggers() {
        let mut det = LoopDetector::new(disabled_config());
        for _ in 0..20 {
            det.record_call("echo", r#"{"msg":"hi"}"#, "same", true);
        }
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 10. Mixed tools → no false positive
    #[test]
    fn mixed_tools_no_false_positive() {
        let mut det = LoopDetector::new(default_config());
        det.record_call("file_read", r#"{"path":"a.rs"}"#, "content_a", true);
        det.record_call("shell", r#"{"cmd":"ls"}"#, "file_list", true);
        det.record_call("memory_store", r#"{"key":"x"}"#, "stored", true);
        det.record_call("file_read", r#"{"path":"b.rs"}"#, "content_b", true);
        det.record_call("shell", r#"{"cmd":"cargo test"}"#, "ok", true);
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 11. UTF-8 boundary safety: hash_output must not panic on CJK text
    #[test]
    fn hash_output_utf8_boundary_safe() {
        // Create a string where byte 4096 lands inside a multi-byte char
        // Chinese chars are 3 bytes each, so 1366 chars = 4098 bytes
        let cjk_text: String = "文".repeat(1366); // 4098 bytes
        assert!(cjk_text.len() > super::OUTPUT_HASH_PREFIX_BYTES);

        // This should NOT panic
        let hash1 = super::hash_output(&cjk_text);

        // Different content should produce different hash
        let cjk_text2: String = "字".repeat(1366);
        let hash2 = super::hash_output(&cjk_text2);
        assert_ne!(hash1, hash2);

        // Mixed ASCII + CJK at boundary
        let mixed = "a".repeat(4094) + "文文"; // 4094 + 6 = 4100 bytes, boundary at 4096
        let hash3 = super::hash_output(&mixed);
        assert!(hash3 != 0); // Just verify it runs
    }

    // 12. elfClaw: Action spam — warn after first success, hard-stop after second
    #[test]
    fn action_spam_warns_after_first_success_and_stops_after_second() {
        let mut det = LoopDetector::new(default_config());
        // 1st success → count reaches limit (1) → InjectWarning
        det.record_call("send_voice", r#"{"text":"sorry"}"#, "sent", true);
        match det.check() {
            DetectionVerdict::InjectWarning(msg) => {
                assert!(msg.contains("send_voice"), "msg: {msg}");
                assert!(msg.contains("already been called"), "msg: {msg}");
            }
            other => panic!("expected InjectWarning, got {other:?}"),
        }
        // 2nd success → count > limit → HardStop
        det.record_call("send_voice", r#"{"text":"sorry again"}"#, "sent", true);
        match det.check() {
            DetectionVerdict::HardStop(msg) => {
                assert!(msg.contains("send_voice"), "msg: {msg}");
                assert!(msg.contains("2 times"), "msg: {msg}");
            }
            other => panic!("expected HardStop, got {other:?}"),
        }
    }

    // 13. elfClaw: Action spam does not fire for non-action tools
    #[test]
    fn action_spam_does_not_fire_for_non_action_tools() {
        let mut det = LoopDetector::new(LoopDetectionConfig {
            // raise other thresholds to isolate spam detection
            no_progress_threshold: 10,
            failure_streak_threshold: 10,
            ping_pong_cycles: 10,
            action_success_limit: 1,
        });
        // shell succeeds many times — should NOT trigger spam watchdog
        for i in 0..5 {
            let args = format!(r#"{{"cmd":"task{i}"}}"#);
            det.record_call("shell", &args, &format!("ok {i}"), true);
        }
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }

    // 14. elfClaw: Action spam disabled when action_success_limit = 0
    #[test]
    fn action_spam_disabled_when_limit_zero() {
        let mut det = LoopDetector::new(LoopDetectionConfig {
            no_progress_threshold: 10,
            ping_pong_cycles: 10,
            failure_streak_threshold: 10,
            action_success_limit: 0,
        });
        for _ in 0..5 {
            det.record_call("send_voice", r#"{"text":"x"}"#, "sent", true);
        }
        assert_eq!(det.check(), DetectionVerdict::Continue);
    }
}
