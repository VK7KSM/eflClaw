//! The few Telegram Bot API calls monitoring pushes need, with what the chat
//! channel's own senders do not give back: the message id (to reply the video
//! under its alarm and to edit a caption later), silent delivery, and an
//! inline buttons ("已看到", "标注", names). Plain text only — no parse mode
//! to escape for.

use anyhow::{bail, Context, Result};
use reqwest::multipart::{Form, Part};
use std::time::Duration;

#[derive(Clone)]
pub struct Bot {
    token: String,
    http: reqwest::Client,
}

/// Inline buttons, row by row: (label, callback data).
pub type Buttons = Vec<Vec<(String, String)>>;

/// A single row of buttons.
pub fn row(buttons: &[(&str, &str)]) -> Buttons {
    if buttons.is_empty() {
        return Vec::new();
    }
    vec![buttons
        .iter()
        .map(|(l, d)| ((*l).to_string(), (*d).to_string()))
        .collect()]
}

/// Options shared by every send.
#[derive(Debug, Clone, Default)]
pub struct SendOpts {
    pub silent: bool,
    /// The message this one replies to.
    pub parent: Option<i64>,
    pub buttons: Buttons,
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

    fn keyboard(buttons: &Buttons) -> serde_json::Value {
        let rows: Vec<Vec<serde_json::Value>> = buttons
            .iter()
            .map(|r| {
                r.iter()
                    .map(|(label, data)| serde_json::json!({"text": label, "callback_data": data}))
                    .collect()
            })
            .collect();
        serde_json::json!({ "inline_keyboard": rows })
    }

    fn markup(buttons: &Buttons) -> Option<String> {
        (!buttons.is_empty()).then(|| Self::keyboard(buttons).to_string())
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

    fn form(chat: &str, opts: &SendOpts) -> Form {
        let mut form = Form::new()
            .text("chat_id", chat.to_string())
            .text("disable_notification", opts.silent.to_string());
        if let Some(id) = opts.parent {
            form = form
                .text("reply_to_message_id", id.to_string())
                .text("allow_sending_without_reply", "true");
        }
        if let Some(m) = Self::markup(&opts.buttons) {
            form = form.text("reply_markup", m);
        }
        form
    }

    pub async fn send_text(&self, chat: &str, text: &str, opts: &SendOpts) -> Result<i64> {
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
        if !opts.buttons.is_empty() {
            body["reply_markup"] = Self::keyboard(&opts.buttons);
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
        opts: &SendOpts,
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
        opts: &SendOpts,
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
    /// message that still needs its buttons passes them again.
    pub async fn edit(
        &self,
        chat: &str,
        message_id: i64,
        text: &str,
        is_photo: bool,
        buttons: &Buttons,
    ) -> Result<()> {
        let (method, field) = if is_photo {
            ("editMessageCaption", "caption")
        } else {
            ("editMessageText", "text")
        };
        let mut body = serde_json::json!({"chat_id": chat, "message_id": message_id, field: text});
        if !buttons.is_empty() {
            body["reply_markup"] = Self::keyboard(buttons);
        }
        self.post_edit(method, &body).await
    }

    /// Show a persistent reply keyboard (the panel) with a short message.
    pub async fn send_keyboard(&self, chat: &str, text: &str, rows: &[Vec<String>]) -> Result<i64> {
        let keyboard: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|r| r.iter().map(|t| serde_json::json!({"text": t})).collect())
            .collect();
        let body = serde_json::json!({
            "chat_id": chat,
            "text": text,
            "disable_notification": true,
            "reply_markup": {"keyboard": keyboard, "resize_keyboard": true, "is_persistent": true},
        });
        let resp = self
            .http
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await?;
        Self::finish(resp, "sendMessage").await
    }

    /// Delete a message the bot sent (Telegram allows it within 48 hours).
    pub async fn delete(&self, chat: &str, message_id: i64) -> Result<()> {
        let body = serde_json::json!({"chat_id": chat, "message_id": message_id});
        self.post_edit("deleteMessage", &body).await
    }

    /// Replace only the buttons of a message (empty: remove them).
    pub async fn set_buttons(&self, chat: &str, message_id: i64, buttons: &Buttons) -> Result<()> {
        let body = serde_json::json!({
            "chat_id": chat,
            "message_id": message_id,
            "reply_markup": Self::keyboard(buttons),
        });
        self.post_edit("editMessageReplyMarkup", &body).await
    }

    async fn post_edit(&self, method: &str, body: &serde_json::Value) -> Result<()> {
        let resp = self.http.post(self.url(method)).json(body).send().await?;
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
