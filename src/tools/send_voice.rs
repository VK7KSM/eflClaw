use super::traits::{Tool, ToolResult};
use crate::config::TtsConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tracing::info;

const TELEGRAM_API_TIMEOUT_SECS: u64 = 15;

/// Agent tool for sending voice messages to Telegram via Edge TTS synthesis.
///
/// The agent calls this tool when it wants to reply with a voice message
/// instead of plain text. The tool synthesizes the text into speech using
/// Microsoft Edge TTS, sends the voice to the chat, and also sends the
/// text version as a follow-up message.
pub struct SendVoiceTool {
    security: Arc<SecurityPolicy>,
    tts_config: TtsConfig,
    bot_token: String,
}

impl SendVoiceTool {
    pub fn new(security: Arc<SecurityPolicy>, tts_config: TtsConfig, bot_token: String) -> Self {
        Self {
            security,
            tts_config,
            bot_token,
        }
    }
}

#[async_trait]
impl Tool for SendVoiceTool {
    fn name(&self) -> &str {
        "send_voice"
    }

    fn description(&self) -> &str {
        "Send a voice message to a Telegram chat. The text you provide will be \
         synthesized into speech using Microsoft Edge TTS and sent as a voice \
         message, followed by the text version. Use this when the user asks you \
         to speak, send a voice message, or when a voice reply is more appropriate \
         than text."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "chat_id": {
                    "type": "string",
                    "description": "The recipient's Telegram chat ID (numeric, e.g. '495916105')"
                },
                "text": {
                    "type": "string",
                    "description": "The text content to synthesize into speech and send as voice message"
                }
            },
            "required": ["chat_id", "text"]
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

        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'text' parameter"))?
            .to_string();

        // Step 1: Synthesize text to speech
        let audio_path = match crate::channels::tts::synthesize(&text, &self.tts_config).await {
            Ok(path) => path,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("TTS synthesis failed: {e}")),
                });
            }
        };

        // Step 2: Send voice message to Telegram
        if let Err(e) =
            crate::channels::tts::send_voice_to_telegram(&self.bot_token, &chat_id, &audio_path)
                .await
        {
            crate::channels::tts::cleanup(&audio_path).await;
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to send voice message: {e}")),
            });
        }

        // Step 3: Cleanup temp audio file
        crate::channels::tts::cleanup(&audio_path).await;

        // Step 4: Send the text version as a follow-up message
        let text_url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let client = crate::config::build_runtime_proxy_client_with_timeouts(
            "tool.send_voice",
            TELEGRAM_API_TIMEOUT_SECS,
            10,
        );

        let payload = json!({
            "chat_id": chat_id,
            "text": format!("🗣️ {text}"),
        });

        // Best-effort text follow-up — don't fail the whole tool if this fails
        match client.post(&text_url).json(&payload).send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(
                    "Voice text follow-up failed (status {}), voice was sent successfully",
                    resp.status()
                );
            }
            Err(e) => {
                tracing::warn!("Voice text follow-up failed: {e}, voice was sent successfully");
            }
            _ => {}
        }

        info!("Voice message sent to {chat_id} ({} chars)", text.len());
        Ok(ToolResult {
            success: true,
            output: format!(
                "Voice message sent successfully to chat_id {chat_id} ({} chars synthesized)",
                text.len()
            ),
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

    fn test_tool(security: Arc<SecurityPolicy>) -> SendVoiceTool {
        SendVoiceTool::new(security, TtsConfig::default(), "test-token".into())
    }

    #[test]
    fn send_voice_tool_name() {
        let tool = test_tool(test_security(AutonomyLevel::Full, 100));
        assert_eq!(tool.name(), "send_voice");
    }

    #[test]
    fn send_voice_tool_has_required_params() {
        let tool = test_tool(test_security(AutonomyLevel::Full, 100));
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("chat_id")));
        assert!(required.contains(&json!("text")));
    }

    #[tokio::test]
    async fn send_voice_blocks_readonly() {
        let tool = test_tool(test_security(AutonomyLevel::ReadOnly, 100));
        let result = tool
            .execute(json!({"chat_id": "123", "text": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn send_voice_blocks_rate_limit() {
        let tool = test_tool(test_security(AutonomyLevel::Full, 0));
        let result = tool
            .execute(json!({"chat_id": "123", "text": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }
}
