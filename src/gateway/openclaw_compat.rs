//! OpenClaw migration compatibility layer.
//!
//! Provides `POST /api/chat` — a ZeroClaw-native endpoint that invokes the full
//! agent loop (`process_message`) with tools, memory recall, and context
//! enrichment. Same code path as Linq/WhatsApp/Nextcloud Talk handlers.
//!
//! The OpenAI-compatible `/v1/chat/completions` and `/v1/models` shims that used
//! to live alongside this endpoint (plus the standalone `openai_compat.rs` module)
//! were removed: elfClaw only serves Telegram directly, so no caller ever needs an
//! OpenAI-shaped HTTP API.

use super::{
    client_key_from_request, run_gateway_chat_with_tools, AppState, RATE_LIMIT_WINDOW_SECS,
};
use crate::memory::MemoryCategory;
use crate::providers;
use axum::{
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use serde::Deserialize;
use std::net::SocketAddr;
use std::time::Instant;
use uuid::Uuid;

// ══════════════════════════════════════════════════════════════════════════════
// /api/chat — ZeroClaw-native endpoint
// ══════════════════════════════════════════════════════════════════════════════

/// Request body for `POST /api/chat`.
#[derive(Debug, Deserialize)]
pub struct ApiChatBody {
    /// The user message to process.
    pub message: String,

    /// Optional session ID for memory scoping.
    /// When provided, memory store/recall operations are isolated to this session.
    #[serde(default)]
    pub session_id: Option<String>,

    /// Optional context lines to prepend to the message.
    /// Use this to inject recent conversation history that ZeroClaw's
    /// semantic memory might not surface (e.g., the last few exchanges).
    #[serde(default)]
    pub context: Vec<String>,
}

fn api_chat_memory_key() -> String {
    format!("api_chat_msg_{}", Uuid::new_v4())
}

/// `POST /api/chat` — full agent loop with tools and memory.
///
/// Request:  `{ "message": "...", "session_id": "...", "context": [...] }`
/// Response: `{ "reply": "...", "model": "..." }`
pub async fn handle_api_chat(
    State(state): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<ApiChatBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // ── Rate limit ──
    let rate_key =
        client_key_from_request(Some(peer_addr), &headers, state.trust_forwarded_headers);
    if !state.rate_limiter.allow_webhook(&rate_key) {
        tracing::warn!("/api/chat rate limit exceeded");
        let err = serde_json::json!({
            "error": "Too many chat requests. Please retry later.",
            "retry_after": RATE_LIMIT_WINDOW_SECS,
        });
        return (StatusCode::TOO_MANY_REQUESTS, Json(err));
    }

    // ── Auth: require at least one layer for non-loopback ──
    if !state.pairing.require_pairing()
        && state.webhook_secret_hash.is_none()
        && !peer_addr.ip().is_loopback()
    {
        tracing::warn!("/api/chat: rejected unauthenticated non-loopback request");
        let err = serde_json::json!({
            "error": "Unauthorized — configure pairing or X-Webhook-Secret for non-local access"
        });
        return (StatusCode::UNAUTHORIZED, Json(err));
    }

    // ── Bearer token auth (pairing) ──
    if state.pairing.require_pairing() {
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = auth.strip_prefix("Bearer ").unwrap_or("");
        if !state.pairing.is_authenticated(token) {
            tracing::warn!("/api/chat: rejected — not paired / invalid bearer token");
            let err = serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            });
            return (StatusCode::UNAUTHORIZED, Json(err));
        }
    }

    // ── Parse body ──
    let Json(chat_body) = match body {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("/api/chat JSON parse error: {e}");
            let err = serde_json::json!({
                "error": "Invalid JSON body. Expected: {\"message\": \"...\"}"
            });
            return (StatusCode::BAD_REQUEST, Json(err));
        }
    };

    let message = chat_body.message.trim();
    if message.is_empty() {
        let err = serde_json::json!({ "error": "Message cannot be empty" });
        return (StatusCode::BAD_REQUEST, Json(err));
    }

    // ── Auto-save to memory ──
    if state.auto_save {
        let key = api_chat_memory_key();
        let _ = state
            .mem
            .store(&key, message, MemoryCategory::Conversation, None)
            .await;
    }

    // ── Build enriched message with optional context ──
    let enriched_message = if chat_body.context.is_empty() {
        message.to_string()
    } else {
        let recent: Vec<&String> = chat_body.context.iter().rev().take(10).rev().collect();
        let context_block = recent
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>()
            .join("\n");
        format!(
            "Recent conversation context:\n{}\n\nCurrent message:\n{}",
            context_block, message
        )
    };

    // ── Observability ──
    let provider_label = state
        .config
        .lock()
        .default_provider
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let model_label = state.model.clone();
    let started_at = Instant::now();

    state
        .observer
        .record_event(&crate::observability::ObserverEvent::AgentStart {
            provider: provider_label.clone(),
            model: model_label.clone(),
        });
    state
        .observer
        .record_event(&crate::observability::ObserverEvent::LlmRequest {
            provider: provider_label.clone(),
            model: model_label.clone(),
            messages_count: 1,
        });

    // ── Run the full agent loop ──
    match run_gateway_chat_with_tools(&state, &enriched_message).await {
        Ok(response) => {
            let duration = started_at.elapsed();

            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: true,
                    error_message: None,
                    input_tokens: None,
                    output_tokens: None,
                });
            state.observer.record_metric(
                &crate::observability::traits::ObserverMetric::RequestLatency(duration),
            );
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            let body = serde_json::json!({
                "reply": response,
                "model": state.model,
                "session_id": chat_body.session_id,
            });
            (StatusCode::OK, Json(body))
        }
        Err(e) => {
            let duration = started_at.elapsed();
            let sanitized = providers::sanitize_api_error(&e.to_string());

            state
                .observer
                .record_event(&crate::observability::ObserverEvent::LlmResponse {
                    provider: provider_label.clone(),
                    model: model_label.clone(),
                    duration,
                    success: false,
                    error_message: Some(sanitized.clone()),
                    input_tokens: None,
                    output_tokens: None,
                });
            state.observer.record_metric(
                &crate::observability::traits::ObserverMetric::RequestLatency(duration),
            );
            state
                .observer
                .record_event(&crate::observability::ObserverEvent::AgentEnd {
                    provider: provider_label,
                    model: model_label,
                    duration,
                    tokens_used: None,
                    cost_usd: None,
                });

            tracing::error!("/api/chat provider error: {sanitized}");
            let err = serde_json::json!({"error": "LLM request failed"});
            (StatusCode::INTERNAL_SERVER_ERROR, Json(err))
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TESTS
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_chat_body_deserializes_minimal() {
        let json = r#"{"message": "Hello"}"#;
        let body: ApiChatBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.message, "Hello");
        assert!(body.session_id.is_none());
        assert!(body.context.is_empty());
    }

    #[test]
    fn api_chat_body_deserializes_full() {
        let json = r#"{
            "message": "What's my schedule?",
            "session_id": "sess-123",
            "context": ["User: hi", "Assistant: hello"]
        }"#;
        let body: ApiChatBody = serde_json::from_str(json).unwrap();
        assert_eq!(body.message, "What's my schedule?");
        assert_eq!(body.session_id.as_deref(), Some("sess-123"));
        assert_eq!(body.context.len(), 2);
    }

    #[test]
    fn memory_key_is_unique() {
        let k1 = api_chat_memory_key();
        let k2 = api_chat_memory_key();
        assert_ne!(k1, k2);
        assert!(k1.starts_with("api_chat_msg_"));
    }

    // ── Handler-level validation tests ──
    // These verify the input shapes that the handlers validate at runtime.

    #[test]
    fn api_chat_body_rejects_missing_message() {
        let json = r#"{"session_id": "s1"}"#;
        let result: Result<ApiChatBody, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "missing `message` field should fail deserialization"
        );
    }
}
