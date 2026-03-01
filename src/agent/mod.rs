#[allow(clippy::module_inception)]
pub mod agent;
pub mod classifier;
pub mod dispatcher;
pub mod loop_;
pub mod memory_loader;
pub mod prompt;
pub mod research;

#[cfg(test)]
mod tests;

/// elfClaw: Task execution context for model routing.
///
/// Passed to [`run`] to indicate whether a task is user-initiated or
/// machine-initiated. Background tasks automatically fall back to
/// `config.worker_model` (if set) instead of `config.default_model`,
/// keeping inference costs low without per-job configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunContext {
    /// User-initiated conversation via any channel (Telegram, Discord, CLI REPL).
    #[default]
    Interactive,
    /// Machine-initiated background task (cron job, heartbeat, email digest).
    Background,
}

#[allow(unused_imports)]
pub use agent::{Agent, AgentBuilder};
#[allow(unused_imports)]
pub use loop_::{process_message, run};
