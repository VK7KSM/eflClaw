//! "🏠 我在家" (owner's request 2026-10-18): under a camera alarm push, for
//! when the owner is home and the alarm is plainly someone of the family the
//! AI did not recognise. Pressing it
//!
//! - deletes that push and its replies (clip, upgrade) in every chat, so
//!   the bot's history does not fill up with useless alarms;
//! - holds back camera alarms for 30 minutes — except those that ring the
//!   phone (confirmed intrusion), which never wait;
//! - counts as "seen" for a phone alarm waiting on that push.

use super::store::Store;
use super::tg::Bot;
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Callback data prefix of the button.
pub const HOME_PREFIX: &str = "cctv_home:";
/// How long camera alarms are held back after a press.
const HOLD_MINUTES: i64 = 30;
/// kv key: until when camera alarms are held back (RFC 3339).
const HOME_UNTIL: &str = "home_until";
/// Event flag: the owner dismissed this alarm.
pub const DISMISSED: &str = "dismissed";

struct Home {
    bot: Bot,
    workspace: PathBuf,
}

static HOME: OnceLock<Home> = OnceLock::new();

/// Set up the button (cctv link start-up).
pub fn register(bot: Bot, workspace: PathBuf) {
    if HOME.set(Home { bot, workspace }).is_ok() {
        crate::channels::telegram::register_callback_handler(HOME_PREFIX, Arc::new(on_press));
    }
}

/// Are camera alarms held back right now?
pub fn active(store: &Store) -> bool {
    active_at(store, Utc::now())
}

fn active_at(store: &Store, now: DateTime<Utc>) -> bool {
    store
        .get(HOME_UNTIL)
        .ok()
        .flatten()
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .is_some_and(|until| until.with_timezone(&Utc) > now)
}

/// Hold camera alarms back for the next 30 minutes.
fn hold(store: &Store, now: DateTime<Utc>) -> Result<()> {
    let until = now + ChronoDuration::minutes(HOLD_MINUTES);
    store.set(HOME_UNTIL, &until.to_rfc3339())
}

/// The cctv store, for button handlers that only get the press.
pub fn open_store() -> Option<Store> {
    HOME.get().and_then(|h| Store::open(&h.workspace).ok())
}

fn on_press(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let id: i64 = p.data.strip_prefix(HOME_PREFIX)?.parse().ok()?;
    let h = HOME.get()?;
    let user = p.user.to_string();
    tokio::spawn(async move {
        if let Err(e) = dismiss(h, id, &user).await {
            tracing::warn!(event = id, "\"我在家\" not fully applied: {e:#}");
        }
    });
    Some(CallbackAnswer {
        toast: format!(
            "好的，已删除这条推送；{HOLD_MINUTES} 分钟内普通报警不再推送（确认有人闯入等会打电话的照常）"
        ),
        clear_buttons: false,
    })
}

async fn dismiss(h: &Home, id: i64, user: &str) -> Result<()> {
    let store = Store::open(&h.workspace)?;
    hold(&store, Utc::now())?;
    store.set_flag(id, DISMISSED)?;
    let _ = super::alarm::acknowledge(&format!("e{id}"), &format!("{user} 点了我在家"));
    let mut targets: Vec<(String, i64)> = store
        .messages(id)?
        .into_iter()
        .map(|m| (m.chat, m.message_id))
        .collect();
    targets.extend(store.replies(id)?);
    for (chat, message) in targets {
        if let Err(e) = h.bot.delete(&chat, message).await {
            tracing::warn!(event = id, "alarm message not deleted: {e:#}");
        }
    }
    tracing::info!(
        event = id,
        by = user,
        "owner is home: alarm dismissed, camera alarms held"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_holds_camera_alarms_for_half_an_hour() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = Utc::now();
        assert!(!active_at(&s, now));
        hold(&s, now).unwrap();
        assert!(active_at(&s, now + ChronoDuration::minutes(29)));
        assert!(!active_at(&s, now + ChronoDuration::minutes(31)));
    }
}
