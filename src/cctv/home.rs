//! "我在家 / 我出门" (owner's rules, linkage plan v1.3–v1.5, 2026-10-18).
//!
//! - The house counts as empty by default. "我在家" always has a limit —
//!   30 minutes unless the owner picks 2 or 4 hours (never longer) — and
//!   ends by itself; the owner is reminded when it does. "我出门" from any
//!   account ends it at once.
//! - While someone is home, level 2 pushes arrive silently; level 3 still
//!   rings. Nothing is held back: guarding stays cctv's job.
//! - "🏠 我在家" under a camera alarm push (not under one that rang the
//!   phone) also deletes that push and its replies in every chat, and the
//!   clips of alarms pushed before the press are not sent any more.
//! - cctv is told (`/presence`, v1.14) and only records and shows it.

use super::api::CctvApi;
use super::store::Store;
use super::tg::{Bot, Buttons, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// Callback data prefix of the button under a push (`cctv_home:<event>`).
pub const HOME_PREFIX: &str = "cctv_home:";
/// Callback data prefix of a duration choice (`cctv_homefor:<minutes>`).
const HOME_FOR_PREFIX: &str = "cctv_homefor:";
/// Event flag: the owner dismissed this alarm.
pub const DISMISSED: &str = "dismissed";
/// The durations offered, in minutes; the first is the default.
const CHOICES: [i64; 3] = [30, 120, 240];
const MAX_MINUTES: i64 = 240;
/// kv keys: until when someone is home (RFC 3339), who said so, when the
/// last "我在家" was pressed (SQLite time, compared with `pushed.at`), and
/// which expiry was already reminded.
const HOME_UNTIL: &str = "home_until";
const HOME_BY: &str = "home_by";
const HOME_PRESSED: &str = "home_pressed";
const HOME_REMINDED: &str = "home_reminded";

struct Home {
    api: CctvApi,
    bot: Bot,
    workspace: PathBuf,
    tz: Tz,
}

static HOME: OnceLock<Home> = OnceLock::new();

/// Set up the buttons (cctv link start-up).
pub fn register(api: CctvApi, bot: Bot, workspace: PathBuf, tz: Tz) {
    if HOME
        .set(Home {
            api,
            bot,
            workspace,
            tz,
        })
        .is_ok()
    {
        crate::channels::telegram::register_callback_handler(HOME_PREFIX, Arc::new(on_press));
        crate::channels::telegram::register_callback_handler(HOME_FOR_PREFIX, Arc::new(on_choose));
    }
}

/// The cctv store, for button handlers that only get the press.
pub fn open_store() -> Option<Store> {
    HOME.get().and_then(|h| Store::open(&h.workspace).ok())
}

fn until(store: &Store) -> Option<DateTime<Utc>> {
    store
        .get(HOME_UNTIL)
        .ok()
        .flatten()
        .and_then(|v| DateTime::parse_from_rfc3339(&v).ok())
        .map(|t| t.with_timezone(&Utc))
}

/// Is someone home right now?
pub fn active(store: &Store) -> bool {
    active_at(store, Utc::now())
}

fn active_at(store: &Store, now: DateTime<Utc>) -> bool {
    until(store).is_some_and(|u| u > now)
}

/// Someone (`chat`) is home for `minutes` (at most 4 hours) from `now`.
fn set_home(store: &Store, minutes: i64, chat: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let until = now + ChronoDuration::minutes(minutes.clamp(1, MAX_MINUTES));
    store.set(HOME_UNTIL, &until.to_rfc3339())?;
    store.set(HOME_BY, chat)?;
    store.set(HOME_PRESSED, &now.format("%Y-%m-%d %H:%M:%S").to_string())?;
    Ok(until)
}

/// "我出门": nobody home from now on; no reminder follows.
fn set_away(store: &Store, now: DateTime<Utc>) -> Result<()> {
    let now = now.to_rfc3339();
    store.set(HOME_UNTIL, &now)?;
    store.set(HOME_REMINDED, &now)
}

/// Was event `id` pushed before the last "我在家"? Its clip is then no
/// longer wanted.
pub fn pressed_after_push(store: &Store, id: i64) -> bool {
    match store.get(HOME_PRESSED) {
        Ok(Some(at)) => store.pushed_no_later_than(id, &at),
        _ => false,
    }
}

fn duration_buttons() -> Buttons {
    let rows: Vec<(String, String)> = CHOICES
        .iter()
        .map(|m| (format!("🏠 {}", span(*m)), format!("{HOME_FOR_PREFIX}{m}")))
        .collect();
    vec![rows]
}

fn span(minutes: i64) -> String {
    if minutes % 60 == 0 {
        format!("{} 小时", minutes / 60)
    } else {
        format!("{minutes} 分钟")
    }
}

fn clock(h: &Home, t: DateTime<Utc>) -> String {
    t.with_timezone(&h.tz).format("%H:%M").to_string()
}

fn account(user: &str, chat: &str) -> String {
    super::api::account_of(user, chat)
}

async fn tell_cctv(h: &Home, minutes: Option<i64>, by: &str) {
    let r = match minutes {
        Some(m) => h.api.presence(m, by).await,
        None => h.api.presence_away(by).await,
    };
    if let Err(e) = r {
        tracing::debug!("presence not told to cctv: {e:#}");
    }
}

/// "🏠 我在家" under a push: delete it everywhere, home for 30 minutes.
fn on_press(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let id: i64 = p.data.strip_prefix(HOME_PREFIX)?.parse().ok()?;
    let h = HOME.get()?;
    let chat = p.chat_id?.to_string();
    let user = account(p.user, &p.chat_id?.to_string());
    let store = Store::open(&h.workspace).ok()?;
    let until = set_home(&store, CHOICES[0], &chat, Utc::now()).ok()?;
    let _ = store.set_flag(id, DISMISSED);
    let _ = super::alarm::acknowledge(&format!("e{id}"), &format!("{user} 点了我在家"));
    tokio::spawn(async move {
        tell_cctv(h, Some(CHOICES[0]), &user).await;
        super::worker::withdraw_push(&h.bot, &store, id).await;
        tracing::info!(event = id, by = user, "owner is home: alarm deleted");
    });
    Some(CallbackAnswer {
        toast: format!(
            "已删除这条推送，记为在家到 {}（{}）。在家时普通报警静音推送；要更久用菜单 /cctv_home",
            clock(h, until),
            span(CHOICES[0])
        ),
        clear_buttons: false,
    })
}

/// A duration picked under the "在家多久？" or reminder message.
fn on_choose(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let minutes: i64 = p.data.strip_prefix(HOME_FOR_PREFIX)?.parse().ok()?;
    let h = HOME.get()?;
    let (chat, message) = (p.chat_id?.to_string(), p.message_id?);
    let user = account(p.user, &p.chat_id?.to_string());
    let store = Store::open(&h.workspace).ok()?;
    let until = set_home(&store, minutes, &chat, Utc::now()).ok()?;
    let text = format!(
        "🏠 已记为在家，到 {}（{}）。到时会提醒你；出门请按菜单 /cctv_away",
        clock(h, until),
        span(minutes.clamp(1, MAX_MINUTES))
    );
    tokio::spawn(async move {
        tell_cctv(h, Some(minutes.clamp(1, MAX_MINUTES)), &user).await;
        if let Err(e) = h.bot.edit(&chat, message, &text, false, &Vec::new()).await {
            tracing::warn!("home confirmation not shown: {e:#}");
        }
    });
    Some(CallbackAnswer {
        toast: "已记为在家".into(),
        clear_buttons: false,
    })
}

/// Menu `/cctv_home`: ask how long. Answered by its own message (buttons),
/// so the menu reply itself is empty.
pub fn menu_home(chat: &str) -> String {
    let Some(h) = HOME.get() else {
        return "监控还没连上，稍后再试".into();
    };
    let chat = chat.to_string();
    tokio::spawn(async move {
        let opts = SendOpts {
            silent: true,
            parent: None,
            buttons: duration_buttons(),
        };
        if let Err(e) = h
            .bot
            .send_text(&chat, "🏠 在家多久？（最长 4 小时，到时会提醒你）", &opts)
            .await
        {
            tracing::warn!("home choice not sent: {e:#}");
        }
    });
    String::new()
}

/// Menu `/cctv_away`: nobody home from now on (any account).
pub fn menu_away(chat: &str, sender: &str) -> String {
    let Some(h) = HOME.get() else {
        return "监控还没连上，稍后再试".into();
    };
    let Some(store) = open_store() else {
        return "设置失败：打不开监控数据库".into();
    };
    if let Err(e) = set_away(&store, Utc::now()) {
        return format!("设置失败：{e:#}");
    }
    let by = account(sender, chat);
    tokio::spawn(async move { tell_cctv(h, None, &by).await });
    "🚪 已记为出门：现在按家里没人处理，cctv 只认人脸或车牌认出的家人。".into()
}

/// Once "我在家" has run out, remind whoever set it (once per expiry).
pub async fn remind_if_expired(bot: &Bot, store: &Store) {
    let Some(h) = HOME.get() else { return };
    let Some(u) = until(store) else { return };
    let key = u.to_rfc3339();
    if u > Utc::now() || store.get(HOME_REMINDED).ok().flatten().as_deref() == Some(key.as_str()) {
        return;
    }
    let _ = store.set(HOME_REMINDED, &key);
    let Ok(Some(chat)) = store.get(HOME_BY) else {
        return;
    };
    let opts = SendOpts {
        silent: true,
        parent: None,
        buttons: duration_buttons(),
    };
    let text = format!(
        "🏠 在家时间到了（{}），现在按家里没人处理。还在家的话选个时长：",
        clock(h, u)
    );
    if let Err(e) = bot.send_text(&chat, &text, &opts).await {
        tracing::warn!("home expiry reminder not sent: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_always_ends_and_away_ends_it_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let now = Utc::now();
        assert!(!active_at(&s, now), "nobody home by default");
        set_home(&s, 30, "100", now).unwrap();
        assert!(active_at(&s, now + ChronoDuration::minutes(29)));
        assert!(!active_at(&s, now + ChronoDuration::minutes(31)));
        set_home(&s, 10_000, "100", now).unwrap();
        assert!(
            !active_at(&s, now + ChronoDuration::minutes(241)),
            "never longer than 4 hours"
        );
        set_away(&s, now).unwrap();
        assert!(!active_at(&s, now + ChronoDuration::seconds(1)));
    }

    #[test]
    fn the_account_is_never_empty_or_too_long() {
        assert_eq!(account("zeroclaw_user", "100"), "zeroclaw_user");
        assert_eq!(account("unknown", "100"), "100");
        assert_eq!(account("", "100"), "100");
        assert_eq!(account(&"x".repeat(80), "100").chars().count(), 64);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(span(30), "30 分钟");
        assert_eq!(span(120), "2 小时");
        let b = duration_buttons();
        assert_eq!(b[0].len(), 3);
        assert_eq!(b[0][2].1, "cctv_homefor:240");
    }
}
