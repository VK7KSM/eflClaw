use super::traits::{Tool, ToolResult};
use crate::channels::split_message_for_telegram;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

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

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.send_telegram",
            TELEGRAM_API_TIMEOUT_SECS,
            10,
        );

        // elfClaw: split long messages into chunks that fit Telegram's 4096 char limit
        let chunks = split_message_for_telegram(&message);
        let total = chunks.len();

        for (i, chunk) in chunks.iter().enumerate() {
            // Add continuation markers for multi-chunk messages
            let text = if total == 1 {
                chunk.clone()
            } else if i == 0 {
                format!("{chunk}\n\n_(continues... {}/{total})_", i + 1)
            } else if i < total - 1 {
                format!("_(continued {}/{total})_\n\n{chunk}\n\n_(continues...)_", i + 1)
            } else {
                format!("_(continued {}/{total})_\n\n{chunk}", i + 1)
            };

            if let Err(e) = self.send_one_chunk(&client, &url, &chat_id, &text, i, total).await {
                warn!(
                    chat_id = %chat_id,
                    chunk = i + 1,
                    total = total,
                    error = %e,
                    "send_telegram chunk failed"
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Failed to send chunk {}/{total} to {chat_id}: {e}",
                        i + 1
                    )),
                });
            }

            // Brief pause between chunks to respect rate limits
            if total > 1 && i < total - 1 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }

        Ok(ToolResult {
            success: true,
            output: format!("Message sent to chat_id {chat_id} ({total} chunk(s))"),
            error: None,
        })
    }
}

impl SendTelegramTool {
    /// Send a single chunk with Markdown parse_mode, falling back to plain text
    /// if Telegram can't parse the entities. Logs message_id on success, warns on failure.
    async fn send_one_chunk(
        &self,
        client: &reqwest::Client,
        url: &str,
        chat_id: &str,
        text: &str,
        chunk_index: usize,
        total_chunks: usize,
    ) -> anyhow::Result<()> {
        let payload = json!({
            "chat_id": chat_id,
            "text": text,
            "parse_mode": "Markdown"
        });

        let response = client.post(url).json(&payload).send().await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // If Markdown parse fails, retry without parse_mode
            if body.contains("can't parse entities") {
                let payload_plain = json!({
                    "chat_id": chat_id,
                    "text": text
                });
                let retry = client.post(url).json(&payload_plain).send().await?;
                let retry_status = retry.status();
                let retry_body = retry.text().await.unwrap_or_default();

                if !retry_status.is_success() {
                    // elfClaw: log failure instead of silent return
                    warn!(
                        chat_id = %chat_id,
                        status = %retry_status,
                        chunk = chunk_index + 1,
                        total = total_chunks,
                        "send_telegram failed (plain text fallback)"
                    );
                    anyhow::bail!(
                        "Telegram API returned {retry_status} (plain text fallback)"
                    );
                }

                let message_id = Self::extract_message_id(&retry_body);
                info!(
                    chat_id = %chat_id,
                    message_id = message_id,
                    chunk = chunk_index + 1,
                    total = total_chunks,
                    "Telegram message sent (plain text fallback)"
                );
                return Ok(());
            }

            // elfClaw: log non-Markdown failures with warn! (was silent before)
            warn!(
                chat_id = %chat_id,
                status = %status,
                chunk = chunk_index + 1,
                total = total_chunks,
                "send_telegram failed"
            );
            anyhow::bail!("Telegram API returned {status}");
        }

        let message_id = Self::extract_message_id(&body);
        info!(
            chat_id = %chat_id,
            message_id = message_id,
            chunk = chunk_index + 1,
            total = total_chunks,
            "Telegram message sent"
        );
        Ok(())
    }

    /// Extract message_id from Telegram API response JSON for tracing.
    fn extract_message_id(body: &str) -> i64 {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| v["result"]["message_id"].as_i64())
            .unwrap_or(-1)
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
        let tool =
            SendTelegramTool::new(test_security(AutonomyLevel::Full, 100), "test-token".into());
        assert_eq!(tool.name(), "send_telegram");
    }

    #[test]
    fn send_telegram_tool_has_required_params() {
        let tool =
            SendTelegramTool::new(test_security(AutonomyLevel::Full, 100), "test-token".into());
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
        let tool =
            SendTelegramTool::new(test_security(AutonomyLevel::Full, 0), "test-token".into());
        let result = tool
            .execute(json!({"chat_id": "123", "message": "hi"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }
}
