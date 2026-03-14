// elfClaw: Observer wrapper that writes to SQLite + broadcasts to SSE
//
// Lives in elfclaw_log (not gateway) to avoid channels→gateway circular dependency.

use crate::observability::traits::{Observer, ObserverEvent, ObserverMetric};
use tokio::sync::broadcast;

/// Wraps a base `Observer` and additionally:
/// 1. Serialises events as JSON and broadcasts them to the global SSE event bus.
/// 2. Converts selected events into `LogEntry` records and persists them via `elfclaw_log`.
pub struct ElfClawObserver {
    inner: Box<dyn Observer>,
    event_tx: broadcast::Sender<serde_json::Value>,
}

impl ElfClawObserver {
    pub fn new(inner: Box<dyn Observer>, event_tx: broadcast::Sender<serde_json::Value>) -> Self {
        Self { inner, event_tx }
    }
}

impl Observer for ElfClawObserver {
    fn record_event(&self, event: &ObserverEvent) {
        // 1. Forward to base observer (tracing output)
        self.inner.record_event(event);

        // 2. Serialize to JSON and broadcast to SSE subscribers
        if let Some(json) = serialize_observer_event(event) {
            let _ = self.event_tx.send(json);
        }

        // 3. Write structured log entry to SQLite + JSONL
        if let Some(entry) = observer_event_to_log_entry(event) {
            super::log(entry);
        }
    }

    fn record_metric(&self, metric: &ObserverMetric) {
        self.inner.record_metric(metric);
    }

    fn flush(&self) {
        self.inner.flush();
    }

    fn name(&self) -> &str {
        "elfclaw"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Serialise an `ObserverEvent` into JSON for SSE broadcast.
/// Mirrors the logic from `gateway/sse.rs:BroadcastObserver`.
fn serialize_observer_event(event: &ObserverEvent) -> Option<serde_json::Value> {
    let now = chrono::Utc::now().to_rfc3339();
    match event {
        ObserverEvent::LlmRequest {
            provider, model, ..
        } => Some(serde_json::json!({
            "type": "llm_request",
            "provider": provider,
            "model": model,
            "timestamp": now,
        })),
        ObserverEvent::LlmResponse {
            provider,
            model,
            duration,
            success,
            error_message,
            input_tokens,
            output_tokens,
        } => Some(serde_json::json!({
            "type": "llm_response",
            "provider": provider,
            "model": model,
            "duration_ms": duration.as_millis() as u64,
            "success": success,
            "error": error_message,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "timestamp": now,
        })),
        ObserverEvent::ToolCall {
            tool,
            duration,
            success,
            args,
            error,
        } => Some(serde_json::json!({
            "type": "tool_call",
            "tool": tool,
            "duration_ms": duration.as_millis() as u64,
            "success": success,
            "args": args,
            "error": error,
            "timestamp": now,
        })),
        ObserverEvent::ToolCallStart { tool } => Some(serde_json::json!({
            "type": "tool_call_start",
            "tool": tool,
            "timestamp": now,
        })),
        ObserverEvent::Error { component, message } => Some(serde_json::json!({
            "type": "error",
            "component": component,
            "message": message,
            "timestamp": now,
        })),
        ObserverEvent::AgentStart { provider, model } => Some(serde_json::json!({
            "type": "agent_start",
            "provider": provider,
            "model": model,
            "timestamp": now,
        })),
        ObserverEvent::AgentEnd {
            provider,
            model,
            duration,
            tokens_used,
            cost_usd,
        } => Some(serde_json::json!({
            "type": "agent_end",
            "provider": provider,
            "model": model,
            "duration_ms": duration.as_millis() as u64,
            "tokens_used": tokens_used,
            "cost_usd": cost_usd,
            "timestamp": now,
        })),
        ObserverEvent::ChannelMessage { channel, direction } => Some(serde_json::json!({
            "type": "channel_message",
            "channel": channel,
            "direction": direction,
            "timestamp": now,
        })),
        ObserverEvent::HeartbeatTick => Some(serde_json::json!({
            "type": "heartbeat_tick",
            "timestamp": now,
        })),
        ObserverEvent::TurnComplete => Some(serde_json::json!({
            "type": "turn_complete",
            "timestamp": now,
        })),
    }
}

/// Convert selected `ObserverEvent` variants into `LogEntry` for persistence.
fn observer_event_to_log_entry(event: &ObserverEvent) -> Option<super::types::LogEntry> {
    use super::types::*;
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();

    match event {
        ObserverEvent::AgentStart { provider, model } => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Info,
            category: LogCategory::AgentLifecycle,
            component: "agent".into(),
            message: format!("Agent started: {provider}/{model}"),
            details: serde_json::json!({"provider": provider, "model": model}),
        }),
        ObserverEvent::AgentEnd {
            provider,
            model,
            duration,
            tokens_used,
            cost_usd,
        } => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Info,
            category: LogCategory::AgentLifecycle,
            component: "agent".into(),
            message: format!(
                "Agent ended: {provider}/{model} ({:.1}s)",
                duration.as_secs_f64()
            ),
            details: serde_json::json!({
                "provider": provider, "model": model,
                "duration_ms": duration.as_millis() as u64,
                "tokens_used": tokens_used,
                "cost_usd": cost_usd,
            }),
        }),
        ObserverEvent::LlmRequest {
            provider,
            model,
            messages_count,
        } => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Debug,
            category: LogCategory::LlmCall,
            component: "provider".into(),
            message: format!("LLM request: {provider}/{model} ({messages_count} msgs)"),
            details: serde_json::json!({"provider": provider, "model": model, "messages_count": messages_count}),
        }),
        ObserverEvent::LlmResponse {
            provider,
            model,
            duration,
            success,
            error_message,
            input_tokens,
            output_tokens,
        } => Some(LogEntry {
            id,
            timestamp: ts,
            level: if *success {
                LogLevel::Info
            } else {
                LogLevel::Error
            },
            category: LogCategory::LlmCall,
            component: "provider".into(),
            message: if *success {
                format!(
                    "LLM response: {provider}/{model} ({:.1}s)",
                    duration.as_secs_f64()
                )
            } else {
                format!(
                    "LLM error: {provider}/{model} — {}",
                    error_message.as_deref().unwrap_or("unknown")
                )
            },
            details: serde_json::json!({
                "provider": provider, "model": model,
                "duration_ms": duration.as_millis() as u64,
                "success": success,
                "error": error_message,
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            }),
        }),
        ObserverEvent::ToolCall {
            tool,
            duration,
            success,
            args,
            error,
        } => Some(LogEntry {
            id,
            timestamp: ts,
            level: if *success {
                LogLevel::Info
            } else {
                LogLevel::Warn
            },
            category: LogCategory::ToolCall,
            component: "tool".into(),
            message: format!(
                "Tool {}: {} ({:.0}ms)",
                if *success { "ok" } else { "fail" },
                tool,
                duration.as_millis()
            ),
            details: serde_json::json!({
                "tool": tool,
                "duration_ms": duration.as_millis() as u64,
                "success": success,
                "args": args,
                "error": error,
            }),
        }),
        ObserverEvent::Error { component, message } => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Error,
            category: LogCategory::System,
            component: component.clone(),
            message: message.clone(),
            details: serde_json::Value::Null,
        }),
        ObserverEvent::ChannelMessage { channel, direction } => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Info,
            category: LogCategory::ChannelMessage,
            component: "channel".into(),
            message: format!("Channel {direction}: {channel}"),
            details: serde_json::json!({"channel": channel, "direction": direction}),
        }),
        ObserverEvent::HeartbeatTick => Some(LogEntry {
            id,
            timestamp: ts,
            level: LogLevel::Debug,
            category: LogCategory::Heartbeat,
            component: "heartbeat".into(),
            message: "Heartbeat tick".into(),
            details: serde_json::Value::Null,
        }),
        // ToolCallStart and TurnComplete are transient UI events, not persisted
        ObserverEvent::ToolCallStart { .. } | ObserverEvent::TurnComplete => None,
    }
}
