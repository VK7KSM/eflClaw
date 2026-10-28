//! elfClaw 2026-10-28: elfgate — the OpenAI-compatible gateway on K5
//! (`http://127.0.0.1:8460/v1`, no key; made by the elfcode session, notes
//! in `C:\Dev\elfCode\elfgate\elfgate.md`). It replaced the Antigravity Tools
//! proxy (owner, relayed by cctv: the proxy was uninstalled). Quota and
//! concurrency are shared with cctv and flight.
//!
//! - Model names are elfgate's own and read from `GET /v1/models` (every
//!   5 minutes): a chain model is served under its own name, or else as its
//!   thinking variant for `reasoning_level` (0/1 `-low`, 2 or unset
//!   `-medium`, 3/4 `-high`; K5 has 1, the owner's "low" for 3.8 of
//!   2026-10-12), or else any variant of it. Nothing listed → not served.
//! - No function calling: a request with tools is not served, and goes on
//!   to the official keys unchanged. Images only as `data:` URLs.
//! - "Not served" is [`NotServed`], which the chain skips without resting
//!   elfgate; real errors (`busy`, `quota_low`, timeouts…) rest it.

use super::traits::{ChatMessage, ChatRequest, ChatResponse, ProviderCapabilities, TokenUsage};
use super::Provider;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// How often the model list is read again.
const MODELS_TTL: Duration = Duration::from_secs(300);
/// What elfgate is told to give up after (`X-Elfgate-Timeout-Ms`); our own
/// client waits a little longer so elfgate's 504 arrives first and its slot
/// (4 shared with cctv and flight) is not held for its default 300 s.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(110);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

/// A request elfgate cannot take (tools, a model it does not list, an
/// image that is not a `data:` URL). Not a failure of elfgate.
#[derive(Debug)]
pub struct NotServed(pub &'static str);

impl std::fmt::Display for NotServed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "elfgate does not serve this request ({})", self.0)
    }
}

impl std::error::Error for NotServed {}

pub struct ElfgateProvider {
    base: String,
    http: reqwest::Client,
    models: parking_lot::Mutex<Option<(Instant, Vec<String>)>>,
    /// The thinking suffix preferred, e.g. `-low`.
    thinking: &'static str,
}

impl ElfgateProvider {
    /// `base` like `http://127.0.0.1:8460/v1`; `reasoning_level` as for the
    /// Gemini provider.
    pub fn new(base: &str, reasoning_level: Option<u8>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(CLIENT_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            // A local address: never through a configured HTTP proxy.
            .no_proxy()
            .build()
            .unwrap_or_default();
        Self {
            base: base.trim().trim_end_matches('/').to_string(),
            http,
            models: parking_lot::Mutex::new(None),
            thinking: match reasoning_level {
                Some(0 | 1) => "-low",
                Some(3..) => "-high",
                _ => "-medium",
            },
        }
    }

    async fn listed_models(&self) -> anyhow::Result<Vec<String>> {
        if let Some((at, models)) = self.models.lock().as_ref() {
            if at.elapsed() < MODELS_TTL {
                return Ok(models.clone());
            }
        }
        let response = self
            .http
            .get(format!("{}/models", self.base))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(super::api_error("elfgate", response).await);
        }
        let body: Value = response.json().await?;
        let models: Vec<String> = body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect();
        *self.models.lock() = Some((Instant::now(), models.clone()));
        Ok(models)
    }

    /// elfgate's name for chain model `model`, if it lists one.
    async fn resolve(&self, model: &str) -> anyhow::Result<String> {
        pick_model(model, self.thinking, &self.listed_models().await?)
            .ok_or_else(|| NotServed("model not listed").into())
    }
}

/// `model` itself, else its `thinking` variant, else any `model-…` variant.
fn pick_model(model: &str, thinking: &str, listed: &[String]) -> Option<String> {
    let prefix = format!("{model}-");
    listed
        .iter()
        .find(|m| *m == model)
        .or_else(|| listed.iter().find(|m| **m == format!("{model}{thinking}")))
        .or_else(|| listed.iter().find(|m| m.starts_with(&prefix)))
        .cloned()
}

/// OpenAI-style messages; images in `[IMAGE:…]` markers become `image_url`
/// parts, and must already be `data:` URLs.
///
/// No system message: elfgate keeps a warm agent per system prompt and asks
/// for it to stay fixed (elfgate.md §3.3), while elfClaw's change with the
/// date and context. The system text goes ahead of the first user message
/// instead — elfgate folds the whole conversation into one prompt anyway.
fn to_api_messages(messages: &[ChatMessage]) -> anyhow::Result<Vec<Value>> {
    let system: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.trim())
        .filter(|c| !c.is_empty())
        .collect();
    let mut preface = (!system.is_empty()).then(|| {
        system.join(
            "

",
        )
    });
    let mut out = Vec::with_capacity(messages.len());
    for m in messages.iter().filter(|m| m.role != "system") {
        let content = match (m.role.as_str(), preface.take()) {
            ("user", Some(p)) => format!(
                "{p}

{}",
                m.content
            ),
            (_, p) => {
                preface = p;
                m.content.clone()
            }
        };
        let (text, images) = if m.role == "user" {
            crate::multimodal::parse_image_markers(&content)
        } else {
            (content.clone(), Vec::new())
        };
        if images.is_empty() {
            out.push(json!({"role": m.role, "content": content}));
            continue;
        }
        if images.iter().any(|i| !i.starts_with("data:")) {
            return Err(NotServed("image not a data: URL").into());
        }
        let mut parts = Vec::new();
        if !text.trim().is_empty() {
            parts.push(json!({"type": "text", "text": text.trim()}));
        }
        parts.extend(
            images
                .into_iter()
                .map(|url| json!({"type": "image_url", "image_url": {"url": url}})),
        );
        out.push(json!({"role": m.role, "content": parts}));
    }
    Ok(out)
}

#[async_trait]
impl Provider for ElfgateProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: true,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(s) = system_prompt {
            messages.push(ChatMessage::system(s));
        }
        messages.push(ChatMessage::user(message));
        self.chat_with_history(&messages, model, temperature).await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let response = self.complete(messages, model).await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat_with_tools(
        &self,
        _messages: &[ChatMessage],
        _tools: &[Value],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        Err(NotServed("tools").into())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        if request.tools.is_some_and(|t| !t.is_empty()) {
            return Err(NotServed("tools").into());
        }
        self.complete(request.messages, model).await
    }
}

impl ElfgateProvider {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        model: &str,
    ) -> anyhow::Result<ChatResponse> {
        if messages.iter().any(|m| m.role == "tool") {
            return Err(NotServed("tool results").into());
        }
        let api_messages = to_api_messages(messages)?;
        let sent = self.resolve(model).await?;
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base))
            .header(
                "X-Elfgate-Timeout-Ms",
                REQUEST_TIMEOUT.as_millis().to_string(),
            )
            .json(&json!({"model": sent, "messages": api_messages}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(super::api_error("elfgate", response).await);
        }
        let body: Value = response.json().await?;
        let text = body["choices"][0]["message"]["content"]
            .as_str()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("elfgate returned no text ({sent})"))?
            .to_string();
        let usage = &body["usage"];
        Ok(ChatResponse {
            text: Some(text),
            tool_calls: Vec::new(),
            usage: Some(TokenUsage {
                input_tokens: usage["prompt_tokens"].as_u64(),
                output_tokens: usage["completion_tokens"].as_u64(),
            }),
            reasoning_content: None,
            quota_metadata: None,
            stop_reason: None,
            raw_stop_reason: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed() -> Vec<String> {
        [
            "agy-stt",
            "gemini-3.8-flash-low",
            "gemini-3.8-flash-medium",
            "gemini-3.8-flash-high",
            "claude-sonnet-4-6",
            "gpt-oss-120b-medium",
        ]
        .map(String::from)
        .to_vec()
    }

    #[test]
    fn chain_models_map_to_elfgates_names() {
        let l = listed();
        assert_eq!(
            pick_model("gemini-3.8-flash", "-medium", &l).as_deref(),
            Some("gemini-3.8-flash-medium")
        );
        assert_eq!(
            pick_model("claude-sonnet-4-6", "-low", &l).as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            pick_model("gpt-oss-120b-medium", "-low", &l).as_deref(),
            Some("gpt-oss-120b-medium")
        );
        assert_eq!(
            pick_model("gemini-3.8-flash", "-low", &l).as_deref(),
            Some("gemini-3.8-flash-low")
        );
        assert_eq!(pick_model("gemini-3.6-flash", "-low", &l), None);
        assert_eq!(
            pick_model("gemini-3.8", "-medium", &l).as_deref(),
            Some("gemini-3.8-flash-low"),
            "any variant when no -medium"
        );
    }

    #[test]
    fn the_system_prompt_goes_ahead_of_the_first_user_message() {
        let m = [
            ChatMessage::system("今天是 10-28"),
            ChatMessage::user("写一条推送"),
            ChatMessage::assistant("好的"),
            ChatMessage::user("再短一点"),
        ];
        let out = to_api_messages(&m).unwrap();
        assert_eq!(out.len(), 3, "no system message");
        assert_eq!(out[0]["role"], "user");
        assert_eq!(
            out[0]["content"],
            "今天是 10-28

写一条推送"
        );
        assert_eq!(out[2]["content"], "再短一点");
        let plain = to_api_messages(&[ChatMessage::user("hi")]).unwrap();
        assert_eq!(plain[0]["content"], "hi");
    }

    #[test]
    fn images_go_as_data_urls_only() {
        let m = [ChatMessage::user(
            "看这张\n[IMAGE:data:image/png;base64,abcd]",
        )];
        let out = to_api_messages(&m).unwrap();
        assert_eq!(out[0]["content"][0]["text"], "看这张");
        assert_eq!(
            out[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,abcd"
        );
        let file = [ChatMessage::user("[IMAGE:/tmp/a.png]")];
        let err = to_api_messages(&file).unwrap_err();
        assert!(err.downcast_ref::<NotServed>().is_some());
    }

    #[tokio::test]
    async fn tool_requests_are_not_served() {
        let p = ElfgateProvider::new("http://127.0.0.1:9/v1", None);
        let err = p
            .chat_with_tools(&[ChatMessage::user("hi")], &[json!({})], "m", 0.0)
            .await
            .unwrap_err();
        assert!(err.downcast_ref::<NotServed>().is_some());
    }
}
