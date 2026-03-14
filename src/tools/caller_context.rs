//! Caller context propagation via `tokio::task_local!`.
//!
//! Each channel message is processed in a dedicated tokio task (see
//! `run_message_dispatch_loop` worker spawn). The `CALLER_INFO` task-local
//! lets tools discover which channel/sender triggered the current request
//! without threading extra parameters through the entire call chain.

tokio::task_local! {
    /// Per-task caller identity, set by the message dispatch loop.
    pub static CALLER_INFO: CallerInfo;
}

/// Identity of the user who triggered the current tool invocation.
#[derive(Clone, Debug)]
pub struct CallerInfo {
    /// Channel name (e.g. "telegram", "discord").
    pub channel: String,
    /// Sender identifier within that channel (e.g. Telegram user ID).
    pub sender: String,
}

/// Read the current caller info from the task-local, if set.
pub fn current_caller() -> Option<CallerInfo> {
    CALLER_INFO.try_with(|info| info.clone()).ok()
}
