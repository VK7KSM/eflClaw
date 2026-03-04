// elfClaw: global logger + event bus + convenience functions
//
// Provides a single-init log store backed by SQLite + JSONL,
// a global SSE event bus, and short-hand helpers for common log patterns.

pub mod observer;
pub mod store;
pub mod types;

pub use observer::ElfClawObserver;
pub use types::{LogCategory, LogEntry, LogLevel};

use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};
use tokio::sync::broadcast;

// ── Global singletons ────────────────────────────────────────────

/// Global log store (initialised once on daemon startup).
static LOGGER: LazyLock<RwLock<Option<Arc<store::LogStore>>>> =
    LazyLock::new(|| RwLock::new(None));

/// Global SSE event bus shared by gateway, channels, and agent.
static GLOBAL_EVENT_TX: LazyLock<broadcast::Sender<serde_json::Value>> =
    LazyLock::new(|| broadcast::channel(512).0);

// ── Public API ───────────────────────────────────────────────────

/// Initialise the log store. Call once at startup (idempotent).
pub fn init(workspace_dir: &Path) {
    match store::LogStore::init(workspace_dir) {
        Ok(s) => {
            let mut guard = LOGGER.write().unwrap_or_else(|e| e.into_inner());
            *guard = Some(Arc::new(s));
            tracing::info!("elfclaw_log: SQLite log store initialised");
        }
        Err(e) => {
            tracing::warn!("elfclaw_log: failed to init log store: {e}");
        }
    }
}

/// Get the global SSE event bus sender.
pub fn global_event_tx() -> broadcast::Sender<serde_json::Value> {
    GLOBAL_EVENT_TX.clone()
}

/// Wrap a base `Observer` with elfClaw logging + SSE broadcast.
pub fn wrap_observer(
    base: Box<dyn crate::observability::Observer>,
) -> ElfClawObserver {
    ElfClawObserver::new(base, global_event_tx())
}

/// Write a log entry to SQLite + JSONL (best-effort, never panics).
pub fn log(entry: LogEntry) {
    let guard = LOGGER.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref store) = *guard {
        store.write(&entry);
    }
    // Also emit via tracing so terminal output is visible
    match entry.level {
        LogLevel::Debug => tracing::debug!(
            category = entry.category.as_str(),
            component = %entry.component,
            "{}",
            entry.message
        ),
        LogLevel::Info => tracing::info!(
            category = entry.category.as_str(),
            component = %entry.component,
            "{}",
            entry.message
        ),
        LogLevel::Warn => tracing::warn!(
            category = entry.category.as_str(),
            component = %entry.component,
            "{}",
            entry.message
        ),
        LogLevel::Error => tracing::error!(
            category = entry.category.as_str(),
            component = %entry.component,
            "{}",
            entry.message
        ),
    }
}

// ── Convenience helpers ──────────────────────────────────────────

/// Log a tool call result.
pub fn log_tool_call(
    tool: &str,
    args_summary: &str,
    success: bool,
    duration_ms: u64,
    error: Option<&str>,
) {
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: if success { LogLevel::Info } else { LogLevel::Warn },
        category: LogCategory::ToolCall,
        component: "agent_loop".into(),
        message: format!(
            "Tool {}: {} ({}ms){}",
            if success { "ok" } else { "fail" },
            tool,
            duration_ms,
            error.map(|e| format!(" — {e}")).unwrap_or_default()
        ),
        details: serde_json::json!({
            "tool": tool,
            "args_summary": truncate(args_summary, 200),
            "success": success,
            "duration_ms": duration_ms,
            "error": error,
        }),
    });
}

/// Log a cron job lifecycle event.
pub fn log_cron_event(
    job_id: &str,
    job_name: &str,
    event: &str,
    details: serde_json::Value,
) {
    let level = if event == "failed" {
        LogLevel::Error
    } else {
        LogLevel::Info
    };
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level,
        category: LogCategory::CronJob,
        component: "cron_scheduler".into(),
        message: format!("Cron {event}: {job_name} ({job_id})"),
        details,
    });
}

/// Log a channel message event (incoming or outgoing).
pub fn log_channel_message(
    channel: &str,
    direction: &str,
    sender: &str,
    target: &str,
) {
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: LogLevel::Info,
        category: LogCategory::ChannelMessage,
        component: "channel".into(),
        message: format!("Channel {direction}: {channel} ({sender} → {target})"),
        details: serde_json::json!({
            "channel": channel,
            "direction": direction,
            "sender": sender,
            "target": target,
        }),
    });
}

/// Log agent session start.
pub fn log_agent_start(provider: &str, model: &str, context: &str) {
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: LogLevel::Info,
        category: LogCategory::AgentLifecycle,
        component: "agent_loop".into(),
        message: format!("Agent start: {provider}/{model} ({context})"),
        details: serde_json::json!({
            "provider": provider,
            "model": model,
            "context": context,
        }),
    });
}

/// Log agent session end.
pub fn log_agent_end(
    provider: &str,
    model: &str,
    duration_ms: u64,
    tokens: Option<u64>,
) {
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: LogLevel::Info,
        category: LogCategory::AgentLifecycle,
        component: "agent_loop".into(),
        message: format!("Agent end: {provider}/{model} ({}ms)", duration_ms),
        details: serde_json::json!({
            "provider": provider,
            "model": model,
            "duration_ms": duration_ms,
            "tokens": tokens,
        }),
    });
}

/// Log an error from any component.
pub fn log_error(
    component: &str,
    message: &str,
    details: serde_json::Value,
) {
    log(LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: LogLevel::Error,
        category: LogCategory::System,
        component: component.into(),
        message: message.into(),
        details,
    });
}

/// Format an RFC3339 timestamp as `[MM-DD HH:MM]` for chat display.
pub fn format_chat_timestamp(rfc3339_ts: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc3339_ts) {
        let local = dt.with_timezone(&chrono::Local);
        format!("[{:02}-{:02} {:02}:{:02}]",
            local.format("%m"), local.format("%d"),
            local.format("%H"), local.format("%M"))
    } else {
        String::new()
    }
}

/// Format a Unix timestamp (seconds since epoch) as `[MM-DD HH:MM]`.
pub fn format_unix_timestamp(unix_secs: u64) -> String {
    use chrono::TimeZone;
    if let Some(dt) = chrono::Local.timestamp_opt(unix_secs as i64, 0).single() {
        format!("[{:02}-{:02} {:02}:{:02}]",
            dt.format("%m"), dt.format("%d"),
            dt.format("%H"), dt.format("%M"))
    } else {
        String::new()
    }
}

// ── Public query API ─────────────────────────────────────────────

// elfClaw: exposes query_recent() so tools (check_logs) can read the log DB without shell commands
/// Query recent log entries from the SQLite store.
/// Used by the `check_logs` tool so the agent can inspect runtime logs without shell commands.
pub fn query_recent(
    limit: usize,
    level_filter: Option<&str>,
    category_filter: Option<&str>,
    since_minutes: Option<u64>,
) -> Vec<LogEntry> {
    let guard = LOGGER.read().unwrap_or_else(|e| e.into_inner());
    if let Some(ref s) = *guard {
        match s.query_recent(limit, level_filter, category_filter, since_minutes) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("elfclaw_log: query_recent failed: {e}");
                vec![]
            }
        }
    } else {
        vec![]
    }
}

fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
