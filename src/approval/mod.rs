//! Interactive approval workflow for supervised mode.
//!
//! Provides a pre-execution hook that prompts the user before tool calls,
//! with session-scoped "Always" allowlists and audit logging.
//!
//! For non-CLI channels (Telegram, Discord, etc.), approval requests are
//! stored as pending items with a 30-minute expiry. The channel handler sends
//! a prompt to the user; when the user responds, `resolve_non_cli_request` is
//! called to record the decision and unblock the waiting tool call.

use crate::config::AutonomyConfig;
use crate::security::AutonomyLevel;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};

// ── Types ────────────────────────────────────────────────────────

/// A request to approve a tool call before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: serde_json::Value,
}

/// The user's response to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalResponse {
    /// Execute this one call.
    Yes,
    /// Deny this call.
    No,
    /// Execute and add tool to session-scoped allowlist.
    Always,
}

/// A single audit log entry for an approval decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalLogEntry {
    pub timestamp: String,
    pub tool_name: String,
    pub arguments_summary: String,
    pub decision: ApprovalResponse,
    pub channel: String,
}

/// A pending approval request for non-CLI channels (Telegram, Discord, etc.).
///
/// Created when a supervised tool call is requested while the user is not at
/// a CLI. The request has a 30-minute expiry; if no response is received
/// before expiry, the tool call is auto-denied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingNonCliApprovalRequest {
    /// Unique ID for this pending request (UUID-like random string).
    pub request_id: String,
    /// Tool name being requested.
    pub tool_name: String,
    /// Tool arguments.
    pub arguments: serde_json::Value,
    /// Channel name (e.g. `"telegram"`, `"discord"`).
    pub channel: String,
    /// Channel-specific reply target (e.g. Telegram chat_id).
    pub reply_target: String,
    /// When this request was created.
    pub created_at: DateTime<Utc>,
    /// When this request expires (created_at + 30 minutes).
    pub expires_at: DateTime<Utc>,
}

impl PendingNonCliApprovalRequest {
    /// Returns true if this request has passed its expiry time.
    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }

    /// Returns the remaining time until expiry in seconds (0 if expired).
    pub fn remaining_secs(&self) -> i64 {
        let diff = (self.expires_at - Utc::now()).num_seconds();
        diff.max(0)
    }
}

/// Error returned when a pending non-CLI approval request cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingApprovalError {
    /// No request with the given ID exists.
    NotFound,
    /// The request existed but has already expired.
    Expired,
    /// The channel of the resolution does not match the request.
    ChannelMismatch,
}

impl std::fmt::Display for PendingApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "pending approval request not found"),
            Self::Expired => write!(f, "pending approval request has expired"),
            Self::ChannelMismatch => write!(f, "channel mismatch for pending approval request"),
        }
    }
}

// ── ApprovalManager ──────────────────────────────────────────────

/// Manages the interactive approval workflow.
///
/// - Checks config-level `auto_approve` / `always_ask` lists
/// - Maintains a session-scoped "always" allowlist
/// - Records an audit trail of all decisions
/// - For non-CLI channels: manages pending approval requests with expiry
pub struct ApprovalManager {
    /// Tools that never need approval (from config).
    auto_approve: HashSet<String>,
    /// Tools that always need approval, ignoring session allowlist.
    always_ask: HashSet<String>,
    /// Autonomy level from config.
    autonomy_level: AutonomyLevel,
    /// Session-scoped allowlist built from "Always" responses.
    session_allowlist: Mutex<HashSet<String>>,
    /// Audit trail of approval decisions.
    audit_log: Mutex<Vec<ApprovalLogEntry>>,
    /// Pending non-CLI approval requests (keyed by request_id).
    pending_non_cli: Mutex<HashMap<String, PendingNonCliApprovalRequest>>,
}

impl ApprovalManager {
    /// Create from autonomy config.
    pub fn from_config(config: &AutonomyConfig) -> Self {
        Self {
            auto_approve: config.auto_approve.iter().cloned().collect(),
            always_ask: config.always_ask.iter().cloned().collect(),
            autonomy_level: config.level,
            session_allowlist: Mutex::new(HashSet::new()),
            audit_log: Mutex::new(Vec::new()),
            pending_non_cli: Mutex::new(HashMap::new()),
        }
    }

    /// Check whether a tool call requires interactive approval.
    ///
    /// Returns `true` if the call needs a prompt, `false` if it can proceed.
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        // Full autonomy never prompts.
        if self.autonomy_level == AutonomyLevel::Full {
            return false;
        }

        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }

        // always_ask overrides everything.
        if self.always_ask.contains(tool_name) {
            return true;
        }

        // auto_approve skips the prompt.
        if self.auto_approve.contains(tool_name) {
            return false;
        }

        // Session allowlist (from prior "Always" responses).
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) {
            return false;
        }

        // Default: supervised mode requires approval.
        true
    }

    /// Record an approval decision and update session state.
    pub fn record_decision(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        decision: ApprovalResponse,
        channel: &str,
    ) {
        // If "Always", add to session allowlist.
        if decision == ApprovalResponse::Always {
            let mut allowlist = self.session_allowlist.lock();
            allowlist.insert(tool_name.to_string());
        }

        // Append to audit log.
        let summary = summarize_args(args);
        let entry = ApprovalLogEntry {
            timestamp: Utc::now().to_rfc3339(),
            tool_name: tool_name.to_string(),
            arguments_summary: summary,
            decision,
            channel: channel.to_string(),
        };
        let mut log = self.audit_log.lock();
        log.push(entry);
    }

    /// Get a snapshot of the audit log.
    pub fn audit_log(&self) -> Vec<ApprovalLogEntry> {
        self.audit_log.lock().clone()
    }

    /// Get the current session allowlist.
    pub fn session_allowlist(&self) -> HashSet<String> {
        self.session_allowlist.lock().clone()
    }

    /// Prompt the user on the CLI and return their decision.
    ///
    /// For non-CLI channels, returns `Yes` automatically (interactive
    /// approval is only supported on CLI for now).
    pub fn prompt_cli(&self, request: &ApprovalRequest) -> ApprovalResponse {
        prompt_cli_interactive(request)
    }

    // ── Non-CLI pending approval ──────────────────────────────────

    /// Create a new pending non-CLI approval request.
    ///
    /// Returns the `request_id` which should be embedded in the approval
    /// message sent to the user so they can reference it in their response.
    pub fn create_non_cli_request(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        channel: &str,
        reply_target: &str,
    ) -> String {
        // Generate a compact random ID using timestamp + random bytes.
        let now = Utc::now();
        let nonce: u32 = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut h = DefaultHasher::new();
            now.timestamp_nanos_opt().unwrap_or_default().hash(&mut h);
            tool_name.hash(&mut h);
            channel.hash(&mut h);
            h.finish() as u32
        };
        let request_id = format!("apr_{:016x}_{nonce:08x}", now.timestamp_micros());

        // Expire after 30 minutes.
        let expires_at = now + chrono::Duration::minutes(30);

        let request = PendingNonCliApprovalRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.to_string(),
            arguments,
            channel: channel.to_string(),
            reply_target: reply_target.to_string(),
            created_at: now,
            expires_at,
        };

        let mut pending = self.pending_non_cli.lock();
        pending.insert(request_id.clone(), request);

        request_id
    }

    /// Attempt to resolve a pending non-CLI approval request.
    ///
    /// On success, records the decision in the audit log and removes the
    /// pending request. Returns the resolved request for the caller to use.
    ///
    /// Fails if the request is not found, expired, or the channel doesn't match.
    pub fn resolve_non_cli_request(
        &self,
        request_id: &str,
        decision: ApprovalResponse,
        resolving_channel: &str,
    ) -> Result<PendingNonCliApprovalRequest, PendingApprovalError> {
        let mut pending = self.pending_non_cli.lock();

        let req = pending
            .get(request_id)
            .ok_or(PendingApprovalError::NotFound)?
            .clone();

        if req.is_expired() {
            pending.remove(request_id);
            return Err(PendingApprovalError::Expired);
        }

        if req.channel != resolving_channel {
            return Err(PendingApprovalError::ChannelMismatch);
        }

        pending.remove(request_id);
        drop(pending); // Release lock before recording decision.

        self.record_decision(&req.tool_name, &req.arguments, decision, &req.channel);

        Ok(req)
    }

    /// Look up a pending non-CLI request by ID without consuming it.
    pub fn get_pending_non_cli_request(
        &self,
        request_id: &str,
    ) -> Option<PendingNonCliApprovalRequest> {
        self.pending_non_cli.lock().get(request_id).cloned()
    }

    /// Return all pending (non-expired) requests for a given channel.
    pub fn pending_requests_for_channel(&self, channel: &str) -> Vec<PendingNonCliApprovalRequest> {
        self.pending_non_cli
            .lock()
            .values()
            .filter(|r| r.channel == channel && !r.is_expired())
            .cloned()
            .collect()
    }

    /// Remove all expired pending non-CLI requests.
    ///
    /// Should be called periodically (e.g. once per heartbeat tick).
    pub fn expire_stale_requests(&self) {
        let mut pending = self.pending_non_cli.lock();
        pending.retain(|_, req| !req.is_expired());
    }

    /// Return the count of currently pending (non-expired) non-CLI requests.
    pub fn pending_non_cli_count(&self) -> usize {
        self.pending_non_cli
            .lock()
            .values()
            .filter(|r| !r.is_expired())
            .count()
    }
}

// ── CLI prompt ───────────────────────────────────────────────────

/// Display the approval prompt and read user input from stdin.
fn prompt_cli_interactive(request: &ApprovalRequest) -> ApprovalResponse {
    let summary = summarize_args(&request.arguments);
    eprintln!();
    eprintln!("🔧 Agent wants to execute: {}", request.tool_name);
    eprintln!("   {summary}");
    eprint!("   [Y]es / [N]o / [A]lways for {}: ", request.tool_name);
    let _ = io::stderr().flush();

    let stdin = io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return ApprovalResponse::No;
    }

    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::Yes,
        "a" | "always" => ApprovalResponse::Always,
        _ => ApprovalResponse::No,
    }
}

/// Produce a short human-readable summary of tool arguments.
fn summarize_args(args: &serde_json::Value) -> String {
    match args {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => truncate_for_summary(s, 80),
                        other => {
                            let s = other.to_string();
                            truncate_for_summary(&s, 80)
                        }
                    };
                    format!("{k}: {val}")
                })
                .collect();
            parts.join(", ")
        }
        other => {
            let s = other.to_string();
            truncate_for_summary(&s, 120)
        }
    }
}

fn truncate_for_summary(input: &str, max_chars: usize) -> String {
    let mut chars = input.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        input.to_string()
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AutonomyConfig;

    fn supervised_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Supervised,
            auto_approve: vec!["file_read".into(), "memory_recall".into()],
            always_ask: vec!["shell".into()],
            ..AutonomyConfig::default()
        }
    }

    fn full_config() -> AutonomyConfig {
        AutonomyConfig {
            level: AutonomyLevel::Full,
            ..AutonomyConfig::default()
        }
    }

    // ── needs_approval ───────────────────────────────────────

    #[test]
    fn auto_approve_tools_skip_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(!mgr.needs_approval("file_read"));
        assert!(!mgr.needs_approval("memory_recall"));
    }

    #[test]
    fn always_ask_tools_always_prompt() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn unknown_tool_needs_approval_in_supervised() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));
        assert!(mgr.needs_approval("http_request"));
    }

    #[test]
    fn full_autonomy_never_prompts() {
        let mgr = ApprovalManager::from_config(&full_config());
        assert!(!mgr.needs_approval("shell"));
        assert!(!mgr.needs_approval("file_write"));
        assert!(!mgr.needs_approval("anything"));
    }

    #[test]
    fn readonly_never_prompts() {
        let config = AutonomyConfig {
            level: AutonomyLevel::ReadOnly,
            ..AutonomyConfig::default()
        };
        let mgr = ApprovalManager::from_config(&config);
        assert!(!mgr.needs_approval("shell"));
    }

    // ── session allowlist ────────────────────────────────────

    #[test]
    fn always_response_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        assert!(mgr.needs_approval("file_write"));

        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "test.txt"}),
            ApprovalResponse::Always,
            "cli",
        );

        // Now file_write should be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn always_ask_overrides_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Even after "Always" for shell, it should still prompt.
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Always,
            "cli",
        );

        // shell is in always_ask, so it still needs approval.
        assert!(mgr.needs_approval("shell"));
    }

    #[test]
    fn yes_response_does_not_add_to_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "file_write",
            &serde_json::json!({}),
            ApprovalResponse::Yes,
            "cli",
        );
        assert!(mgr.needs_approval("file_write"));
    }

    // ── audit log ────────────────────────────────────────────

    #[test]
    fn audit_log_records_decisions() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "rm -rf ./build/"}),
            ApprovalResponse::No,
            "cli",
        );
        mgr.record_decision(
            "file_write",
            &serde_json::json!({"path": "out.txt", "content": "hello"}),
            ApprovalResponse::Yes,
            "cli",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[1].tool_name, "file_write");
        assert_eq!(log[1].decision, ApprovalResponse::Yes);
    }

    #[test]
    fn audit_log_contains_timestamp_and_channel() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        mgr.record_decision(
            "shell",
            &serde_json::json!({"command": "ls"}),
            ApprovalResponse::Yes,
            "telegram",
        );

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert!(!log[0].timestamp.is_empty());
        assert_eq!(log[0].channel, "telegram");
    }

    // ── summarize_args ───────────────────────────────────────

    #[test]
    fn summarize_args_object() {
        let args = serde_json::json!({"command": "ls -la", "cwd": "/tmp"});
        let summary = summarize_args(&args);
        assert!(summary.contains("command: ls -la"));
        assert!(summary.contains("cwd: /tmp"));
    }

    #[test]
    fn summarize_args_truncates_long_values() {
        let long_val = "x".repeat(200);
        let args = serde_json::json!({ "content": long_val });
        let summary = summarize_args(&args);
        assert!(summary.contains('…'));
        assert!(summary.len() < 200);
    }

    #[test]
    fn summarize_args_unicode_safe_truncation() {
        let long_val = "🦀".repeat(120);
        let args = serde_json::json!({ "content": long_val });
        let summary = summarize_args(&args);
        assert!(summary.contains("content:"));
        assert!(summary.contains('…'));
    }

    #[test]
    fn summarize_args_non_object() {
        let args = serde_json::json!("just a string");
        let summary = summarize_args(&args);
        assert!(summary.contains("just a string"));
    }

    // ── ApprovalResponse serde ───────────────────────────────

    #[test]
    fn approval_response_serde_roundtrip() {
        let json = serde_json::to_string(&ApprovalResponse::Always).unwrap();
        assert_eq!(json, "\"always\"");
        let parsed: ApprovalResponse = serde_json::from_str("\"no\"").unwrap();
        assert_eq!(parsed, ApprovalResponse::No);
    }

    // ── ApprovalRequest ──────────────────────────────────────

    #[test]
    fn approval_request_serde() {
        let req = ApprovalRequest {
            tool_name: "shell".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ApprovalRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tool_name, "shell");
    }

    // ── PendingNonCliApprovalRequest ──────────────────────────────

    #[test]
    fn pending_non_cli_create_and_resolve() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request_id = mgr.create_non_cli_request(
            "shell",
            serde_json::json!({"command": "ls"}),
            "telegram",
            "123456",
        );
        assert!(!request_id.is_empty());

        // Should appear in pending list.
        let pending = mgr.pending_requests_for_channel("telegram");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tool_name, "shell");

        // Resolve it.
        let resolved = mgr
            .resolve_non_cli_request(&request_id, ApprovalResponse::Yes, "telegram")
            .unwrap();
        assert_eq!(resolved.tool_name, "shell");
        assert_eq!(resolved.channel, "telegram");

        // Should no longer be pending.
        let pending = mgr.pending_requests_for_channel("telegram");
        assert!(pending.is_empty());
    }

    #[test]
    fn pending_non_cli_not_found() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let result =
            mgr.resolve_non_cli_request("nonexistent_id", ApprovalResponse::Yes, "telegram");
        assert_eq!(result.unwrap_err(), PendingApprovalError::NotFound);
    }

    #[test]
    fn pending_non_cli_channel_mismatch() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request_id = mgr.create_non_cli_request(
            "file_write",
            serde_json::json!({}),
            "telegram",
            "123",
        );

        // Trying to resolve on a different channel should fail.
        let result = mgr.resolve_non_cli_request(&request_id, ApprovalResponse::No, "discord");
        assert_eq!(result.unwrap_err(), PendingApprovalError::ChannelMismatch);

        // Request should still be pending after channel mismatch.
        assert_eq!(mgr.pending_non_cli_count(), 1);
    }

    #[test]
    fn pending_non_cli_always_adds_to_session_allowlist() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request_id = mgr.create_non_cli_request(
            "file_write",
            serde_json::json!({"path": "out.txt"}),
            "telegram",
            "123",
        );

        mgr.resolve_non_cli_request(&request_id, ApprovalResponse::Always, "telegram")
            .unwrap();

        // file_write should now be in session allowlist.
        assert!(!mgr.needs_approval("file_write"));
    }

    #[test]
    fn pending_non_cli_expire_stale_removes_expired() {
        let mgr = ApprovalManager::from_config(&supervised_config());

        // Manually insert an already-expired request.
        {
            let past = Utc::now() - chrono::Duration::hours(1);
            let req = PendingNonCliApprovalRequest {
                request_id: "expired_req".into(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({}),
                channel: "telegram".into(),
                reply_target: "123".into(),
                created_at: past,
                expires_at: past, // already expired
            };
            mgr.pending_non_cli.lock().insert("expired_req".into(), req);
        }

        assert_eq!(mgr.pending_non_cli_count(), 0); // is_expired() filters it
        mgr.expire_stale_requests(); // Should not panic; removes the entry.

        assert!(mgr.pending_non_cli.lock().is_empty());
    }

    #[test]
    fn pending_non_cli_get_by_id() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request_id = mgr.create_non_cli_request(
            "http_request",
            serde_json::json!({"url": "https://example.com"}),
            "discord",
            "ch_999",
        );

        let fetched = mgr.get_pending_non_cli_request(&request_id).unwrap();
        assert_eq!(fetched.tool_name, "http_request");
        assert_eq!(fetched.reply_target, "ch_999");
        assert!(!fetched.is_expired());
        assert!(fetched.remaining_secs() > 1700); // > 28 minutes remaining
    }

    #[test]
    fn pending_non_cli_resolve_records_audit_log() {
        let mgr = ApprovalManager::from_config(&supervised_config());
        let request_id = mgr.create_non_cli_request(
            "shell",
            serde_json::json!({"command": "echo hello"}),
            "telegram",
            "999",
        );

        mgr.resolve_non_cli_request(&request_id, ApprovalResponse::No, "telegram")
            .unwrap();

        let log = mgr.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].tool_name, "shell");
        assert_eq!(log[0].decision, ApprovalResponse::No);
        assert_eq!(log[0].channel, "telegram");
    }

    // ── PendingApprovalError display ──────────────────────────────

    #[test]
    fn pending_approval_error_display() {
        assert_eq!(
            PendingApprovalError::NotFound.to_string(),
            "pending approval request not found"
        );
        assert_eq!(
            PendingApprovalError::Expired.to_string(),
            "pending approval request has expired"
        );
        assert_eq!(
            PendingApprovalError::ChannelMismatch.to_string(),
            "channel mismatch for pending approval request"
        );
    }
}
