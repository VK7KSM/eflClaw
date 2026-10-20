//! elfClaw 2026-10-20: disarming needs the owner's PIN (cctv v1.15.4 §9.1).
//!
//! Every disarm — the panel's "全部撤防", switching the roof antenna's AI
//! tracking off, the `cctv_arm` tool — goes through [`ask`]: the program
//! asks the chat for the PIN, and the next text message of that chat is
//! taken by [`intercept`] before the channel logs, stores or answers it. A
//! PIN-looking message is deleted from the chat at once and used for one
//! disarm request; it never reaches the model, the chat log, the history or
//! a log line. The model can ask for a disarm but never sees or handles
//! the PIN.

use super::api::{CctvApi, Disarm};
use super::tg::{row, Bot, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

/// How long a PIN request waits.
const WAIT: Duration = Duration::from_secs(120);
const CANCEL: &str = "cctv_pin:cancel";

/// Run after a successful disarm (e.g. refresh the panel message).
pub type AfterDisarm =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

struct Pending {
    body: Value,
    account: String,
    /// What is being disarmed, for the messages ("全部摄像头").
    what: String,
    /// The "请发送 PIN" message, edited with the outcome.
    prompt: i64,
    until: Instant,
    after: Option<AfterDisarm>,
}

struct Link {
    api: CctvApi,
    bot: Bot,
}

static LINK: OnceLock<Link> = OnceLock::new();
static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn init(api: CctvApi, bot: Bot) {
    if LINK.set(Link { api, bot }).is_err() {
        return;
    }
    crate::channels::telegram::register_message_interceptor("cctv_pin", Arc::new(intercept));
    crate::channels::telegram::register_callback_handler(
        "cctv_pin:",
        Arc::new(|press: &CallbackPress<'_>| {
            let chat = press.chat_id?.to_string();
            let pending = PENDING.lock().remove(&chat);
            if let (Some(p), Some(link)) = (pending, LINK.get()) {
                let text = format!("已取消，{}保持布防。", p.what);
                tokio::spawn(async move {
                    let _ = link
                        .bot
                        .edit(&chat, p.prompt, &text, false, &Vec::new())
                        .await;
                });
            }
            Some(CallbackAnswer {
                toast: "已取消".into(),
                clear_buttons: true,
            })
        }),
    );
}

/// Ask `chat` for the PIN to disarm with `body` (`monitor_id`, `state`,
/// optional `until`; no `pin`). The disarm happens when the PIN arrives.
pub async fn ask(
    chat: &str,
    body: Value,
    account: &str,
    what: &str,
    after: Option<AfterDisarm>,
) -> anyhow::Result<()> {
    let link = LINK
        .get()
        .ok_or_else(|| anyhow::anyhow!("撤防 PIN 功能没有启动"))?;
    let text = format!(
        "🔐 撤防{what}需要 PIN。\n请在 2 分钟内直接发送撤防 PIN（4–8 位数字），收到后我会立刻删掉那条消息。"
    );
    let opts = SendOpts {
        silent: false,
        parent: None,
        buttons: row(&[("取消", CANCEL)]),
    };
    let prompt = link.bot.send_text(chat, &text, &opts).await?;
    PENDING.lock().insert(
        chat.to_string(),
        Pending {
            body,
            account: account.to_string(),
            what: what.to_string(),
            prompt,
            until: Instant::now() + WAIT,
            after,
        },
    );
    Ok(())
}

/// What a message is, while a PIN is awaited.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Pin(String),
    /// Digits, but not 4–8 of them.
    BadLength,
    /// Not a PIN at all: the user moved on.
    Other,
}

fn classify(text: &str) -> Reply {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() || !compact.chars().all(|c| c.is_ascii_digit()) {
        return Reply::Other;
    }
    if (4..=8).contains(&compact.len()) {
        Reply::Pin(compact)
    } else {
        Reply::BadLength
    }
}

/// The channel's interceptor: takes the awaited PIN, lets anything else by.
fn intercept(chat: &str, _sender: &str, message_id: i64, text: &str) -> bool {
    let Some(link) = LINK.get() else {
        return false;
    };
    let pending = {
        let mut all = PENDING.lock();
        all.retain(|_, p| p.until > Instant::now());
        all.remove(chat)
    };
    let Some(pending) = pending else {
        return false;
    };
    let chat = chat.to_string();
    match classify(text) {
        Reply::Other => {
            let note = format!("没收到 PIN，已取消撤防，{}保持布防。", pending.what);
            tokio::spawn(async move {
                let _ = link
                    .bot
                    .edit(&chat, pending.prompt, &note, false, &Vec::new())
                    .await;
            });
            false
        }
        Reply::BadLength => {
            tokio::spawn(async move {
                let _ = link.bot.delete(&chat, message_id).await;
                retry(link, &chat, pending, "PIN 是 4–8 位数字，请重新发送。").await;
            });
            true
        }
        Reply::Pin(pin) => {
            tokio::spawn(async move {
                // Gone from the chat before anything else happens.
                if let Err(e) = link.bot.delete(&chat, message_id).await {
                    tracing::warn!("cctv pin: could not delete the PIN message: {e:#}");
                }
                use_pin(link, &chat, pending, &pin).await;
            });
            true
        }
    }
}

/// Wait for the PIN again (wrong or malformed), with `note` on the prompt.
async fn retry(link: &'static Link, chat: &str, mut pending: Pending, note: &str) {
    let text = format!(
        "🔐 {note}\n撤防{}：请在 2 分钟内发送撤防 PIN。",
        pending.what
    );
    let _ = link
        .bot
        .edit(
            chat,
            pending.prompt,
            &text,
            false,
            &row(&[("取消", CANCEL)]),
        )
        .await;
    pending.until = Instant::now() + WAIT;
    PENDING.lock().insert(chat.to_string(), pending);
}

async fn use_pin(link: &'static Link, chat: &str, mut pending: Pending, pin: &str) {
    let outcome = link.api.disarm(&pending.body, pin, &pending.account).await;
    let done = |text: String| async move {
        let _ = link
            .bot
            .edit(chat, pending.prompt, &text, false, &Vec::new())
            .await;
    };
    match outcome {
        Ok(Disarm::Done) => {
            let after = pending.after.take();
            let what = pending.what.clone();
            done(format!("⏸ 已撤防{what}。")).await;
            if let Some(after) = after {
                after().await;
            }
        }
        Ok(Disarm::PinWrong { remaining }) => {
            let note = match remaining {
                Some(n) => format!("PIN 不对，还能再试 {n} 次。"),
                None => "PIN 不对。".into(),
            };
            retry(link, chat, pending, &note).await;
        }
        Ok(Disarm::Locked { retry_after_s }) => {
            let wait = retry_after_s
                .map(|s| format!("约 {} 分钟后再试", (s + 59) / 60))
                .unwrap_or_else(|| "过一会儿再试".into());
            done(format!(
                "🔒 PIN 错太多次，撤防已锁定，{wait}。{}保持布防。",
                pending.what
            ))
            .await;
        }
        Ok(Disarm::PinNotSet) => {
            done(format!(
                "还没设撤防 PIN，没法撤防。请先在警戒区域页「布置 → 撤防 PIN」里设置。{}保持布防。",
                pending.what
            ))
            .await;
        }
        Err(e) => {
            done(format!("撤防没成功：{e:#}")).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_four_to_eight_digits_are_a_pin() {
        assert_eq!(classify("1234"), Reply::Pin("1234".into()));
        assert_eq!(classify(" 1234 5678 "), Reply::Pin("12345678".into()));
        assert_eq!(classify("123"), Reply::BadLength);
        assert_eq!(classify("123456789"), Reply::BadLength);
        assert_eq!(classify("算了不撤了"), Reply::Other);
        assert_eq!(classify("12ab"), Reply::Other);
        assert_eq!(classify(""), Reply::Other);
    }
}
