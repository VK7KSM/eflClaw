//! The highest alarms — a confirmed intrusion, or two or more cameras down —
//! wake the owner: SMS + phone call (SIP), on top of the Telegram push.
//!
//! The owner has acknowledged when any of these happens (user's rules):
//! said "知道了" on the call, hung up on it, replied to the SMS, or pressed
//! "已看到" in Telegram. Voicemail, no response, a dropped call or a call that
//! never went out (line busy) do not count. Without an acknowledgement, the
//! push, SMS and call are repeated once, two minutes after the first.

use super::label::TAG_PREFIX;
use super::store::Sent;
use super::tg::{row, Bot, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use crate::sip::client::{CallOptions, CallOutcome, Response};
use crate::sip::media::SpeechCheck;
use crate::sip::SipClient;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Callback data prefix of the "已看到" button.
pub const ACK_PREFIX: &str = "cctv_ack:";
const RETRY_AFTER: Duration = Duration::from_secs(120);
const ATTEMPTS: u32 = 2;

pub struct AlarmLine {
    pub bot: Bot,
    pub chats: Vec<String>,
    pub owner: String,
    pub sip: SipClient,
    pub prompt: Arc<Vec<i16>>,
    pub confirm: Option<Arc<Vec<i16>>>,
    pub speech: Option<SpeechCheck>,
}

/// Pending alarms: key → who acknowledged (set once).
static PENDING: Mutex<Option<HashMap<String, watch::Sender<Option<String>>>>> = Mutex::new(None);
/// One alarm sequence at a time: a second qualifying event while one is
/// running is pushed to Telegram but does not start another round of calls.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Record an acknowledgement for `key`. Returns the toast for the button.
pub fn acknowledge(key: &str, by: &str) -> Option<String> {
    let guard = PENDING.lock();
    let tx = guard.as_ref()?.get(key)?;
    if tx.borrow().is_none() {
        let _ = tx.send(Some(by.to_string()));
    }
    Some("已确认，谢谢".to_string())
}

pub fn is_pending(key: &str) -> bool {
    PENDING
        .lock()
        .as_ref()
        .and_then(|m| m.get(key))
        .is_some_and(|tx| tx.borrow().is_none())
}

/// The "已看到" press itself — `None` when the data is not ours.
pub fn ack_press(data: &str, user: &str) -> Option<String> {
    let key = data.strip_prefix(ACK_PREFIX)?;
    acknowledge(key, &format!("{user} 在 Telegram 点了已看到"))
        .or(Some("这次报警已经处理过了".to_string()))
}

/// Handler for the Telegram channel's button registry. The pressed push
/// keeps its "标注" button: only "已看到" goes.
pub fn telegram_callback(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let toast = ack_press(p.data, p.user)?;
    let event = p
        .data
        .strip_prefix(ACK_PREFIX)
        .and_then(|k| k.strip_prefix('e'))
        .and_then(|id| id.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let (Some(bot), Some(event), Some(chat), Some(message)) =
        (BOT.get(), event, p.chat_id, p.message_id)
    else {
        return Some(CallbackAnswer {
            toast,
            clear_buttons: true,
        });
    };
    let tag = format!("{TAG_PREFIX}{event}");
    let buttons = row(&[("🏷 标注", tag.as_str())]);
    let bot = bot.clone();
    tokio::spawn(async move {
        if let Err(e) = bot.set_buttons(&chat.to_string(), message, &buttons).await {
            tracing::warn!("acknowledged push buttons not updated: {e:#}");
        }
    });
    Some(CallbackAnswer {
        toast,
        clear_buttons: false,
    })
}

/// The bot that updates a push's buttons after "已看到" (set at start-up).
pub static BOT: std::sync::OnceLock<Bot> = std::sync::OnceLock::new();

/// Digits only, +61 → 0, so "+61444…" and "0444…" compare equal.
pub fn same_number(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let d: String = s.chars().filter(char::is_ascii_digit).collect();
        match d.strip_prefix("61") {
            Some(rest) if d.len() == 11 => format!("0{rest}"),
            _ => d,
        }
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
}

fn outcome_zh(o: &CallOutcome) -> String {
    match o {
        CallOutcome::Responded(Response::Voice) => "电话里说了确认".into(),
        CallOutcome::Responded(Response::Digit) => "电话里按了确认键".into(),
        CallOutcome::Responded(Response::HungUp) => "接了电话后挂断".into(),
        CallOutcome::Voicemail => "转进了语音信箱".into(),
        CallOutcome::NoResponse => "没人接".into(),
        CallOutcome::Dropped => "电话中途断了".into(),
        CallOutcome::NotConnected { code: 486, .. } => "没打出去（线路占用）".into(),
        CallOutcome::NotConnected { code, reason } => format!("没打出去（{code} {reason}）"),
    }
}

/// Start the alarm sequence for `key` (Telegram already pushed, with the
/// button, as `origin`). Returns false when another sequence is running.
pub fn start(line: Arc<AlarmLine>, key: String, headline: String, origin: Vec<Sent>) -> bool {
    if BUSY.swap(true, Ordering::SeqCst) {
        tracing::warn!(key = %key, "alarm sequence already running; Telegram only");
        return false;
    }
    let (tx, rx) = watch::channel(None);
    PENDING
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(key.clone(), tx);
    tokio::spawn(async move {
        run(&line, &key, &headline, &origin, rx).await;
        if let Some(m) = PENDING.lock().as_mut() {
            m.remove(&key);
        }
        BUSY.store(false, Ordering::SeqCst);
    });
    true
}

async fn reply_all(line: &AlarmLine, origin: &[Sent], text: &str, silent: bool) {
    for s in origin {
        let opts = SendOpts {
            silent,
            parent: Some(s.message_id),
            buttons: Vec::new(),
        };
        if let Err(e) = line.bot.send_text(&s.chat, text, &opts).await {
            tracing::warn!("alarm notice not delivered: {e:#}");
        }
    }
}

async fn run(
    line: &AlarmLine,
    key: &str,
    headline: &str,
    origin: &[Sent],
    mut acked: watch::Receiver<Option<String>>,
) {
    let button_data = format!("{ACK_PREFIX}{key}");
    let mut by: Option<String> = None;
    for attempt in 1..=ATTEMPTS {
        let deadline = Instant::now() + RETRY_AFTER;
        if attempt > 1 {
            for chat in &line.chats {
                let opts = SendOpts {
                    silent: false,
                    parent: None,
                    buttons: row(&[("✅ 已看到", &button_data)]),
                };
                let text = format!("🚨 再次提醒（还没有人确认）：{headline}");
                if let Err(e) = line.bot.send_text(chat, &text, &opts).await {
                    tracing::warn!("alarm repeat not delivered: {e:#}");
                }
            }
        }
        let mut sms = line.sip.subscribe_sms();
        let text = format!("【家庭监控】最高级报警：{headline}。回复任意内容表示知道了。");
        if let Err(e) = line.sip.send_sms(&line.owner, &text).await {
            tracing::warn!("alarm SMS not sent: {e:#}");
        }
        let call = line.sip.call(
            &line.owner,
            CallOptions {
                prompt: line.prompt.clone(),
                confirm: line.confirm.clone(),
                gap: Duration::from_millis(1200),
                max: Duration::from_secs(40),
                digit: '1',
                speech_check: line.speech.clone(),
            },
        );
        tokio::pin!(call);
        let mut call_done = false;
        loop {
            if let Some(who) = acked.borrow().clone() {
                by = Some(who);
                break;
            }
            tokio::select! {
                outcome = &mut call, if !call_done => {
                    call_done = true;
                    match outcome {
                        Ok(CallOutcome::Responded(r)) => {
                            by = Some(outcome_zh(&CallOutcome::Responded(r)));
                            break;
                        }
                        Ok(other) => {
                            reply_all(line, origin, &format!("📞 第 {attempt} 次电话：{}", outcome_zh(&other)), true).await;
                        }
                        Err(e) => {
                            tracing::warn!("alarm call failed: {e:#}");
                            reply_all(line, origin, &format!("📞 第 {attempt} 次电话没打出去：{e}"), true).await;
                        }
                    }
                }
                msg = sms.recv() => {
                    if let Ok(m) = msg {
                        if same_number(&m.from, &line.owner) {
                            by = Some("回了短信".to_string());
                            break;
                        }
                    }
                }
                _ = acked.changed() => {}
                () = tokio::time::sleep_until(deadline.into()), if call_done => break,
            }
        }
        if by.is_some() {
            break;
        }
    }
    match by {
        Some(who) => {
            let _ = acknowledge(key, &who);
            tracing::info!(key, by = %who, "alarm acknowledged");
            reply_all(line, origin, &format!("✅ 已确认：{who}"), true).await;
        }
        None => {
            tracing::warn!(key, "alarm not acknowledged after {ATTEMPTS} rounds");
            reply_all(line, origin, "⚠️ 两次电话和短信都没有得到确认", false).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_compare_in_both_formats() {
        assert!(same_number("0412345678", "+61412345678"));
        assert!(same_number("61412345678", "0412345678"));
        assert!(!same_number("0412345678", "0412345679"));
        assert!(!same_number("", ""));
    }

    #[test]
    fn acknowledgement_is_recorded_once() {
        let (tx, rx) = watch::channel(None);
        PENDING
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert("t1".into(), tx);
        assert!(is_pending("t1"));
        assert!(ack_press("cctv_ack:t1", "zeroclaw_user").is_some());
        assert_eq!(
            rx.borrow().as_deref(),
            Some("zeroclaw_user 在 Telegram 点了已看到")
        );
        let _ = acknowledge("t1", "回了短信");
        assert_eq!(
            rx.borrow().as_deref(),
            Some("zeroclaw_user 在 Telegram 点了已看到"),
            "first wins"
        );
        assert!(!is_pending("t1"));
        assert_eq!(
            ack_press("cctv_ack:gone", "u").as_deref(),
            Some("这次报警已经处理过了")
        );
        assert_eq!(ack_press("other", "u"), None);
        PENDING.lock().as_mut().unwrap().remove("t1");
    }
}
