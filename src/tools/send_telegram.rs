use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tracing::info;

const TELEGRAM_API_TIMEOUT_SECS: u64 = 15;

/// Agent tool for sending Telegram messages via the Bot API.
pub struct SendTelegramTool {
    security: Arc<SecurityPolicy>,
    bot_token: String,
}

impl SendTelegramTool {
    pub fn new(security: Arc<SecurityPolicy>, bot_token: String) -> Self {
        Self {
            security,
            bot_token,
        }
    }
}

#[async_trait]
impl Tool for SendTelegramTool {
    fn name(&self) -> &str {
        "send_telegram"
    }

    fn description(&self) -> &str {
        "Send a Telegram message to a specific chat. Use this to proactively notify \
         someone via Telegram. You need the recipient's numeric chat ID (not username). \
         Supports Markdown formatting."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "chat_id": {
                    "type": "string",
                    "description": "The recipient's Telegram chat ID (numeric, e.g. '495916105')"
                },
                "message": {
                    "type": "string",
                    "description": "The message text to send. Supports Markdown formatting."
                }
            },
            "required": ["chat_id", "message"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let chat_id = args
            .get("chat_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'chat_id' parameter"))?
            .to_string();

        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'message' parameter"))?
            .to_string();

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.send_telegram",
            TELEGRAM_API_TIMEOUT_SECS,
            10,
        );

        let payload = json!({
            "chat_id": chat_id,
            "text": message,
            "parse_mode": "Markdown"
        });

        let response = client.post(&url).json(&payload).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // If Markdown parse fails, retry without parse_mode
            if body.contains("can't parse entities") {
                let payload_plain = json!({
                    "chat_id": chat_id,
                    "text": message
                });
                let retry = client.post(&url).json(&payload_plain).send().await?;
                let retry_status = retry.status();
                let retry_body = retry.text().await.unwrap_or_default();

                if !retry_status.is_success() {
                    return Ok(ToolResult {
                        success: false,
                        output: retry_body,
                        error: Some(format!("Telegram API returned status {retry_status}")),
                    });
                }

                info!("Telegram message sent to {} (plain text fallback)", chat_id);
                return Ok(ToolResult {
                    success: true,
                    output: format!("Message sent to chat_id {chat_id} (plain text)"),
                    error: None,
                });
            }

            return Ok(ToolResult {
                success: false,
                output: body,
                error: Some(format!("Telegram API returned status {status}")),
            });
        }

        info!("Telegram message sent to {}", chat_id);
        Ok(ToolResult {
            success: true,
            output: format!("Message sent successfully to chat_id {chat_id}"),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;

    fn test_security(level: AutonomyLevel, max_actions_per_hour: u32) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: level,
            max_actions_per_hour,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn send_telegram_tool_name() {
        let tool = SendTelegramTool::new(
            test_security(AutonomyLevel::Full, 100),
            "test-token".into(),
        );
        assert_eq!(tool.name(), "send_telegram");
    }

    #[test]
    fn send_telegram_tool_has_required_params() {
        let tool = SendTelegramTool::new(
            test_security(AutonomyLevel::Full, 100),
            "test-token".into(),
        );
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("chat_id")));
        assert!(required.contains(&json!("message")));
    }

    #[tokio::test]
    async fn send_telegram_blocks_readonly() {
        let tool = SendTelegramTool::new(
            test_security(AutonomyLevel::ReadOnly, 100),
            "test-token".into(),
        );
        let result = tool
            .execute(json!({"chat_id": "123", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn send_telegram_blocks_rate_limit() {
        let tool = SendTelegramTool::new(
            test_security(AutonomyLevel::Full, 0),
            "test-token".into(),
        );
        let result = tool
            .execute(json!({"chat_id": "123", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }
}
