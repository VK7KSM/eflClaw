//! Looking at a camera picture with the chat model (Gemini sees images), so
//! a snapshot always comes with what is in it (owner's rule 2026-10-14: never
//! "I cannot see the picture"). Shared by the `cctv` tool and the panel.

use crate::providers::{self, ChatMessage, ChatRequest, Provider, ProviderRuntimeOptions};
use base64::Engine as _;
use std::fmt::Write as _;
use std::time::Duration;

/// Instructions for looking at a camera picture.
const LOOK_PROMPT: &str =
    "你在看用户自己家里监控摄像头的画面。用中文简洁地说清楚画面里看得到的东西：\
有没有人（几个、在哪、在做什么、穿着）、车辆（停着还是在动）、动物、门和大门的状态、\
任何异常。只说看得到的，看不清就说看不清，不要编。用户有具体问题时先回答问题。";

/// The model that looks at pictures.
pub struct Looker {
    provider: Box<dyn Provider>,
    model: String,
}

impl Looker {
    pub fn from_config(config: &crate::config::Config) -> Option<Self> {
        let options = ProviderRuntimeOptions {
            zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
            secrets_encrypt: config.secrets.encrypt,
            reasoning_level: Some(1),
            ..ProviderRuntimeOptions::default()
        };
        match providers::create_resilient_provider_with_options(
            config.default_provider.as_deref().unwrap_or("gemini"),
            config.api_key.as_deref(),
            config.api_url.as_deref(),
            &config.reliability,
            &options,
        ) {
            Ok(provider) => Some(Self {
                provider,
                model: config
                    .default_model
                    .clone()
                    .unwrap_or_else(|| "gemini-3.5-flash".into()),
            }),
            Err(e) => {
                tracing::warn!("CCTV pictures will not be described: {e:#}");
                None
            }
        }
    }

    /// What the model sees in `jpeg` (`what` says which camera and when).
    /// Never fails: a failure becomes the text returned.
    pub async fn look(&self, jpeg: &[u8], what: &str, question: Option<&str>) -> String {
        let mut user = format!("这是{what}。");
        if let Some(q) = question.map(str::trim).filter(|q| !q.is_empty()) {
            let _ = write!(user, "\n用户的问题：{q}");
        }
        let _ = write!(
            user,
            "\n[IMAGE:data:image/jpeg;base64,{}]",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        let messages = [ChatMessage::system(LOOK_PROMPT), ChatMessage::user(user)];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        match tokio::time::timeout(
            Duration::from_secs(90),
            self.provider.chat(request, &self.model, 0.2),
        )
        .await
        {
            Ok(Ok(r)) => r
                .text
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "（模型没有给出描述）".into()),
            Ok(Err(e)) => format!("（看图失败：{e:#}）"),
            Err(_) => "（看图超时）".into(),
        }
    }
}

/// [`Looker::look`], or a note when no model can look at pictures.
pub async fn describe(
    looker: Option<&Looker>,
    jpeg: &[u8],
    what: &str,
    question: Option<&str>,
) -> String {
    match looker {
        Some(l) => l.look(jpeg, what, question).await,
        None => "（没有能看图的模型，无法分析画面）".into(),
    }
}
