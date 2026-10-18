//! "✅ 有用 / ❌ 没用" under every push that rang the phone (owner's rule,
//! linkage plan §9, contract v1.15 §16). A press
//!
//! - counts as "seen": the second round of push, SMS and call is cancelled;
//! - is reported to cctv (`call_feedback`), which counts useless calls per
//!   rule so the rules can be improved (by the cctv session, with the owner);
//! - "没用" first asks why; when it was family or friends, the label flow
//!   follows so cctv learns to know them.

use super::api::{account_of, CctvApi};
use super::store::Store;
use super::tg::{Bot, Buttons, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use chrono::DateTime;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Callback data prefix: `cctv_fb:<alarm key>:y`, `…:n`, `…:r:<reason>`.
pub const FB_PREFIX: &str = "cctv_fb:";
/// Event flag: feedback given, the buttons do not come back.
pub(super) const FEEDBACK_DONE: &str = "feedback";
/// The reasons cctv accepts, with their labels.
const REASONS: [(&str, &str); 5] = [
    ("family", "是家人"),
    ("neighbor_friend", "是邻居朋友"),
    ("courier_worker", "是快递工人"),
    ("not_person", "不是人"),
    ("other", "别的"),
];

struct Fb {
    api: CctvApi,
    bot: Bot,
    workspace: PathBuf,
}

static FB: OnceLock<Fb> = OnceLock::new();

/// Set up the buttons (cctv link start-up).
pub fn register(api: CctvApi, bot: Bot, workspace: PathBuf) {
    if FB
        .set(Fb {
            api,
            bot,
            workspace,
        })
        .is_ok()
    {
        crate::channels::telegram::register_callback_handler(FB_PREFIX, Arc::new(on_press));
    }
}

/// The feedback row for the alarm `key` (`e<event id>`, or `down<unix
/// seconds>` for "cctv unreachable").
pub fn feedback_row(key: &str) -> Vec<(String, String)> {
    vec![
        ("✅ 有用".into(), format!("{FB_PREFIX}{key}:y")),
        ("❌ 没用".into(), format!("{FB_PREFIX}{key}:n")),
    ]
}

fn reason_buttons(key: &str) -> Buttons {
    let all: Vec<(String, String)> = REASONS
        .iter()
        .map(|(code, zh)| ((*zh).to_string(), format!("{FB_PREFIX}{key}:r:{code}")))
        .collect();
    all.chunks(3).map(<[_]>::to_vec).collect()
}

fn answer(toast: &str) -> Option<CallbackAnswer> {
    Some(CallbackAnswer {
        toast: toast.to_string(),
        clear_buttons: false,
    })
}

/// What a press means: `Ok(None)` = show the reasons, `Ok(Some((useful,
/// reason)))` = report it; `Err(())` = not a feedback press we know.
fn parse(action: &str) -> Result<Option<(bool, Option<&'static str>)>, ()> {
    match action {
        "n" => Ok(None),
        "y" => Ok(Some((true, None))),
        _ => {
            let code = action.strip_prefix("r:").ok_or(())?;
            let (code, _) = REASONS.iter().find(|(c, _)| *c == code).ok_or(())?;
            Ok(Some((false, Some(code))))
        }
    }
}

fn on_press(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let (key, action) = p.data.strip_prefix(FB_PREFIX)?.split_once(':')?;
    let (chat, message) = (p.chat_id?.to_string(), p.message_id?);
    let f = FB.get()?;
    let key = key.to_string();
    let Some((useful, reason)) = parse(action).ok()? else {
        let buttons = reason_buttons(&key);
        tokio::spawn(async move {
            if let Err(e) = f.bot.set_buttons(&chat, message, &buttons).await {
                tracing::warn!("feedback reasons not shown: {e:#}");
            }
        });
        return answer("没用的原因是？");
    };
    let account = account_of(p.user, &chat);
    let said = match reason.and_then(|r| REASONS.iter().find(|(c, _)| *c == r)) {
        Some((_, zh)) => format!("没用（{zh}）"),
        None => "有用".to_string(),
    };
    let _ = super::alarm::acknowledge(&key, &format!("{account} 反馈：{said}"));
    tokio::spawn(async move {
        report(f, &key, useful, reason, &account, &chat, message).await;
    });
    answer(&format!("已记下：{said}，谢谢"))
}

async fn report(
    f: &Fb,
    key: &str,
    useful: bool,
    reason: Option<&str>,
    account: &str,
    chat: &str,
    message: i64,
) {
    let event = key
        .strip_prefix('e')
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let result = match (event, key.strip_prefix("down")) {
        (Some(id), _) => f.api.call_feedback(id, useful, reason, account).await,
        (None, Some(ts)) => {
            let Some(at) = ts
                .parse::<i64>()
                .ok()
                .and_then(|s| DateTime::from_timestamp(s, 0))
            else {
                return;
            };
            f.api
                .call_feedback_unreachable(&at.to_rfc3339(), useful, reason, account)
                .await
                .map(|()| false)
        }
        _ => return,
    };
    match result {
        Ok(ask_label) => {
            let store = Store::open(&f.workspace).ok();
            let buttons = match (event, &store) {
                (Some(id), Some(s)) => {
                    let _ = s.set_flag(id, FEEDBACK_DONE);
                    let taggable = !s.has_flag(id, super::worker::NOTAG);
                    super::worker::plain_buttons(Some(id), taggable, false)
                }
                _ => Vec::new(),
            };
            if let Err(e) = f.bot.set_buttons(chat, message, &buttons).await {
                tracing::warn!("feedback buttons not updated: {e:#}");
            }
            if let (true, Some(id)) = (ask_label, event) {
                super::label::offer_now(id, chat.to_string(), message);
            }
        }
        Err(e) => {
            tracing::warn!("call feedback not recorded: {e:#}");
            let opts = SendOpts {
                silent: true,
                parent: Some(message),
                buttons: Vec::new(),
            };
            let _ = f
                .bot
                .send_text(chat, &format!("反馈没记上：{e:#}，可以再按一次"), &opts)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_map_to_what_cctv_accepts() {
        assert_eq!(parse("y"), Ok(Some((true, None))));
        assert_eq!(parse("n"), Ok(None), "first ask why");
        assert_eq!(parse("r:family"), Ok(Some((false, Some("family")))));
        assert_eq!(
            parse("r:courier_worker"),
            Ok(Some((false, Some("courier_worker"))))
        );
        assert_eq!(parse("r:unknown"), Err(()));
        assert_eq!(parse("x"), Err(()));
    }

    #[test]
    fn buttons_fit_telegram() {
        let row = feedback_row("e123456789");
        assert_eq!(row[0].1, "cctv_fb:e123456789:y");
        let reasons = reason_buttons("down1760000000");
        assert_eq!(reasons.iter().flatten().count(), 5);
        for (_, data) in reasons.iter().flatten().chain(row.iter()) {
            assert!(data.len() <= 64, "{data}");
        }
    }
}
