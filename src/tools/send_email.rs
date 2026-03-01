use super::traits::{Tool, ToolResult};
use crate::channels::email_channel::EmailConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use lettre::message::SinglePart;
use lettre::transport::smtp::authentication::Credentials;
use lettre::Message;
use lettre::SmtpTransport;
use lettre::Transport;
use serde_json::json;
use std::sync::Arc;
use tracing::info;

/// Agent tool for sending emails via the configured SMTP transport.
pub struct SendEmailTool {
    security: Arc<SecurityPolicy>,
    config: EmailConfig,
}

impl SendEmailTool {
    pub fn new(security: Arc<SecurityPolicy>, config: EmailConfig) -> Self {
        Self { security, config }
    }

    fn create_smtp_transport(&self) -> anyhow::Result<SmtpTransport> {
        let creds = Credentials::new(self.config.username.clone(), self.config.password.clone());
        let transport = if self.config.smtp_tls {
            SmtpTransport::relay(&self.config.smtp_host)?
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        } else {
            SmtpTransport::builder_dangerous(&self.config.smtp_host)
                .port(self.config.smtp_port)
                .credentials(creds)
                .build()
        };
        Ok(transport)
    }
}

#[async_trait]
impl Tool for SendEmailTool {
    fn name(&self) -> &str {
        "send_email"
    }

    fn description(&self) -> &str {
        "Send an email via SMTP. Use this to compose and send emails on behalf of the user. \
         The email will be sent from the configured email address. \
         You can specify recipient, subject, and body."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient email address"
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject line"
                },
                "body": {
                    "type": "string",
                    "description": "Email body content (plain text)"
                }
            },
            "required": ["to", "subject", "body"]
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

        let to = args
            .get("to")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'to' parameter"))?
            .to_string();

        let subject = args
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'subject' parameter"))?
            .to_string();

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing 'body' parameter"))?
            .to_string();

        // Build the email message
        let email = match Message::builder()
            .from(self.config.from_address.parse()?)
            .to(to.parse()?)
            .subject(&subject)
            .singlepart(SinglePart::plain(body))
        {
            Ok(email) => email,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build email: {e}")),
                });
            }
        };

        // Send via SMTP
        let transport = match self.create_smtp_transport() {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to create SMTP transport: {e}")),
                });
            }
        };

        match transport.send(&email) {
            Ok(_) => {
                info!("Email sent to {} (subject: {})", to, subject);
                Ok(ToolResult {
                    success: true,
                    output: format!("Email sent successfully to {to}"),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("SMTP send failed: {e}")),
            }),
        }
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

    fn test_config() -> EmailConfig {
        EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 465,
            smtp_tls: true,
            username: "test@example.com".into(),
            password: "test".into(),
            from_address: "test@example.com".into(),
            ..EmailConfig::default()
        }
    }

    #[test]
    fn send_email_tool_name() {
        let tool = SendEmailTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        assert_eq!(tool.name(), "send_email");
    }

    #[test]
    fn send_email_tool_has_required_params() {
        let tool = SendEmailTool::new(test_security(AutonomyLevel::Full, 100), test_config());
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("to")));
        assert!(required.contains(&json!("subject")));
        assert!(required.contains(&json!("body")));
    }

    #[tokio::test]
    async fn send_email_blocks_readonly() {
        let tool = SendEmailTool::new(test_security(AutonomyLevel::ReadOnly, 100), test_config());
        let result = tool
            .execute(json!({"to": "a@b.com", "subject": "hi", "body": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn send_email_blocks_rate_limit() {
        let tool = SendEmailTool::new(test_security(AutonomyLevel::Full, 0), test_config());
        let result = tool
            .execute(json!({"to": "a@b.com", "subject": "hi", "body": "hello"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }
}
