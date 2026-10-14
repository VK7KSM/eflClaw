//! The few Telegram Bot API calls monitoring pushes need, with what the chat
//! channel's own senders do not give back: the message id (to reply the video
//! under its alarm and to edit a caption later), silent delivery, and an
//! inline "已看到" button. Plain text only — no parse mode to escape for.

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use std::time::Duration;

#[derive(Clone)]
pub struct Bot {
    token: String,
    http: reqwest::Client,
}

/// Options shared by every send.
#[derive(Debug, Clone, Default)]
pub struct SendOpts<'a> {
    pub silent: bool,
    /// The message this one replies to.
    pub parent: Option<i64>,
    /// One inline button: (label, callback data).
    pub button: Option<(&'a str, &'a str)>,
}

impl Bot {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.token)
    }

    fn markup(button: Option<(&str, &str)>) -> Option<String> {
        button.map(|(label, data)| {
            serde_json::json!({"inline_keyboard": [[{"text": label, "callback_data": data}]]})
                .to_string()
        })
    }

    async fn finish(resp: reqwest::Response, what: &str) -> Result<i64> {
        let status = resp.status();
        let v: serde_json::Value = resp
            .json()
            .await
            .with_context(|| format!("Telegram {what}: bad reply"))?;
        if !status.is_success() || v["ok"].as_bool() != Some(true) {
            // The description never contains the token.
            bail!(
                "Telegram {what} failed ({}): {}",
                status.as_u16(),
                v["description"].as_str().unwrap_or("")
            );
        }
        v["result"]["message_id"]
            .as_i64()
            .with_context(|| format!("Telegram {what}: no message id"))
    }

    fn form(chat: &str, opts: &SendOpts<'_>) -> Form {
        let mut form = Form::new()
            .text("chat_id", chat.to_string())
            .text("disable_notification", opts.silent.to_string());
        if let Some(id) = opts.parent {
            form = form
                .text("reply_to_message_id", id.to_string())
                .text("allow_sending_without_reply", "true");
        }
        if let Some(m) = Self::markup(opts.button) {
            form = form.text("reply_markup", m);
        }
        form
    }

    pub async fn send_text(&self, chat: &str, text: &str, opts: &SendOpts<'_>) -> Result<i64> {
        let mut body = serde_json::json!({
            "chat_id": chat,
            "text": text,
            "disable_notification": opts.silent,
            "disable_web_page_preview": true,
        });
        if let Some(id) = opts.parent {
            body["reply_to_message_id"] = id.into();
            body["allow_sending_without_reply"] = true.into();
        }
        if let Some(m) = Self::markup(opts.button) {
            body["reply_markup"] = serde_json::from_str(&m)?;
        }
        let resp = self
            .http
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await?;
        Self::finish(resp, "sendMessage").await
    }

    pub async fn send_photo(
        &self,
        chat: &str,
        jpeg: Vec<u8>,
        caption: &str,
        opts: &SendOpts<'_>,
    ) -> Result<i64> {
        let form = Self::form(chat, opts)
            .text("caption", caption.to_string())
            .part(
                "photo",
                Part::bytes(jpeg)
                    .file_name("snapshot.jpg")
                    .mime_str("image/jpeg")?,
            );
        let resp = self
            .http
            .post(self.url("sendPhoto"))
            .multipart(form)
            .send()
            .await?;
        Self::finish(resp, "sendPhoto").await
    }

    pub async fn send_video(
        &self,
        chat: &str,
        mp4: Vec<u8>,
        caption: &str,
        opts: &SendOpts<'_>,
    ) -> Result<i64> {
        let form = Self::form(chat, opts)
            .text("caption", caption.to_string())
            .text("supports_streaming", "true")
            .part(
                "video",
                Part::bytes(mp4)
                    .file_name("clip.mp4")
                    .mime_str("video/mp4")?,
            );
        let resp = self
            .http
            .post(self.url("sendVideo"))
            .multipart(form)
            .send()
            .await?;
        Self::finish(resp, "sendVideo").await
    }

    /// Replace the caption of a photo (or the text of a text message).
    /// Telegram drops an inline keyboard the edit does not repeat, so a
    /// message that still needs its button passes it again.
    pub async fn edit(
        &self,
        chat: &str,
        message_id: i64,
        text: &str,
        is_photo: bool,
        button: Option<(&str, &str)>,
    ) -> Result<()> {
        let (method, field) = if is_photo {
            ("editMessageCaption", "caption")
        } else {
            ("editMessageText", "text")
        };
        let mut body = serde_json::json!({"chat_id": chat, "message_id": message_id, field: text});
        if let Some(m) = Self::markup(button) {
            body["reply_markup"] = serde_json::from_str(&m)?;
        }
        let resp = self.http.post(self.url(method)).json(&body).send().await?;
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        // "message is not modified" is not an error worth reporting.
        if v["ok"].as_bool() != Some(true)
            && !v["description"]
                .as_str()
                .unwrap_or("")
                .contains("not modified")
        {
            bail!(
                "Telegram {method} failed: {}",
                v["description"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }
}
