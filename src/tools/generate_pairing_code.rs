//! Agent tool for remote pairing code generation.
//!
//! Generates a fresh 6-digit gateway pairing code so the user can pair a
//! web client without physical terminal access. Protected by four layers:
//!
//! 1. Channel `allowed_users` whitelist (existing)
//! 2. `always_ask` approval gate (existing)
//! 3. Approval actor identity check (fixed in this PR)
//! 4. Hard-coded caller context check against config whitelist (this tool)

use super::caller_context::current_caller;
use super::traits::{Tool, ToolResult};
use crate::security::pairing::get_global_pairing_guard;
use async_trait::async_trait;
use serde_json::json;

/// Tool for generating a fresh gateway pairing code at runtime.
pub struct GeneratePairingCodeTool {
    allowed_channels: Vec<String>,
    allowed_users: Vec<String>,
}

impl GeneratePairingCodeTool {
    pub fn new(allowed_channels: Vec<String>, allowed_users: Vec<String>) -> Self {
        Self {
            allowed_channels,
            allowed_users,
        }
    }
}

#[async_trait]
impl Tool for GeneratePairingCodeTool {
    fn name(&self) -> &str {
        "generate_pairing_code"
    }

    fn description(&self) -> &str {
        "Generate a fresh 6-digit pairing code for the web client gateway. \
         The new code replaces any previous unused code. Use this when the user \
         needs to pair a new device remotely. Only works in private chat with \
         authorized users."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _params: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Layer 4: hard-coded caller context check
        let caller = match current_caller() {
            Some(c) => c,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: "Permission denied: no caller context available.".to_string(),
                    error: None,
                });
            }
        };

        if !self
            .allowed_channels
            .iter()
            .any(|ch| ch.eq_ignore_ascii_case(&caller.channel))
        {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "Permission denied: channel '{}' is not authorized for pairing code generation.",
                    caller.channel
                ),
                error: None,
            });
        }

        if !self
            .allowed_users
            .iter()
            .any(|u| u.eq_ignore_ascii_case(&caller.sender))
        {
            return Ok(ToolResult {
                success: false,
                output: format!(
                    "Permission denied: user '{}' is not authorized for pairing code generation.",
                    caller.sender
                ),
                error: None,
            });
        }

        // Fetch global PairingGuard
        let guard = match get_global_pairing_guard() {
            Some(g) => g,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: "Gateway is not running or pairing guard is not registered.".to_string(),
                    error: None,
                });
            }
        };

        // Generate new code
        match guard.force_regenerate_code() {
            Ok(code) => Ok(ToolResult {
                success: true,
                output: format!(
                    "New pairing code: {}\n\n\
                     This code is valid until used or replaced. \
                     Enter it on the web client pairing page to connect.\n\
                     ⚠️ Do not share this code — it grants device access.",
                    code
                ),
                error: None,
            }),
            Err(msg) => Ok(ToolResult {
                success: false,
                output: format!("Failed to generate pairing code: {msg}"),
                error: Some(msg.to_string()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_without_caller_context() {
        let tool = GeneratePairingCodeTool::new(
            vec!["telegram".to_string()],
            vec!["alice".to_string()],
        );
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("no caller context"));
    }

    #[tokio::test]
    async fn rejects_unauthorized_channel() {
        use super::super::caller_context::{CallerInfo, CALLER_INFO};

        let tool = GeneratePairingCodeTool::new(
            vec!["telegram".to_string()],
            vec!["alice".to_string()],
        );

        let caller = CallerInfo {
            channel: "discord".to_string(),
            sender: "alice".to_string(),
        };
        let result = CALLER_INFO
            .scope(caller, async { tool.execute(json!({})).await.unwrap() })
            .await;
        assert!(!result.success);
        assert!(result.output.contains("channel"));
    }

    #[tokio::test]
    async fn rejects_unauthorized_user() {
        use super::super::caller_context::{CallerInfo, CALLER_INFO};

        let tool = GeneratePairingCodeTool::new(
            vec!["telegram".to_string()],
            vec!["alice".to_string()],
        );

        let caller = CallerInfo {
            channel: "telegram".to_string(),
            sender: "mallory".to_string(),
        };
        let result = CALLER_INFO
            .scope(caller, async { tool.execute(json!({})).await.unwrap() })
            .await;
        assert!(!result.success);
        assert!(result.output.contains("user"));
    }

    #[tokio::test]
    async fn succeeds_with_valid_caller_and_guard() {
        use super::super::caller_context::{CallerInfo, CALLER_INFO};
        use crate::security::pairing::{register_global_pairing_guard, PairingGuard};
        use std::sync::Arc;

        // Register a guard with pairing enabled and no existing tokens
        let guard = Arc::new(PairingGuard::new(true, &[]));
        register_global_pairing_guard(guard);

        let tool = GeneratePairingCodeTool::new(
            vec!["telegram".to_string()],
            vec!["alice".to_string()],
        );

        let caller = CallerInfo {
            channel: "telegram".to_string(),
            sender: "alice".to_string(),
        };
        let result = CALLER_INFO
            .scope(caller, async { tool.execute(json!({})).await.unwrap() })
            .await;
        assert!(result.success, "Expected success, got: {}", result.output);
        assert!(result.output.contains("pairing code"));
    }
}
