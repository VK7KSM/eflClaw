// elfClaw: rich structured log entry types for diagnostics
use serde::{Deserialize, Serialize};

/// Log severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Log category for structured filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogCategory {
    AgentLifecycle,
    LlmCall,
    ToolCall,
    CronJob,
    Heartbeat,
    ChannelMessage,
    WorkerStatus,
    System,
}

impl LogCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentLifecycle => "agent_lifecycle",
            Self::LlmCall => "llm_call",
            Self::ToolCall => "tool_call",
            Self::CronJob => "cron_job",
            Self::Heartbeat => "heartbeat",
            Self::ChannelMessage => "channel_message",
            Self::WorkerStatus => "worker_status",
            Self::System => "system",
        }
    }
}

/// A single structured log entry written to SQLite + JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub timestamp: String,
    pub level: LogLevel,
    pub category: LogCategory,
    pub component: String,
    pub message: String,
    pub details: serde_json::Value,
}
