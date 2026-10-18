//! The long-poll loop: cctv-server events → Telegram (and, for the highest
//! alarms, the phone line). Push rules decided with the owner:
//!
//! - level 3: always, with sound; level 2: always, silent 23:00–06:30;
//!   level 1: not pushed, counted in the morning summary; recoveries silent;
//! - the text alone at once (no snapshot: uploading a picture made alarms
//!   slow — owner's decision 2026-10-16), the clip as a reply once ready;
//! - a later rise in level is pushed again; any later change of the text
//!   (the description arriving or growing, e.g. cctv's second-opinion review
//!   appended to it) edits the caption; an AI downgrade to level 1 is noted
//!   on the caption;
//! - cctv-server unreachable for 3 minutes is itself a highest alarm;
//! - of cctv's own summaries only the nightly report is pushed;
//! - while someone is home (`home`, always time-limited) level 2 arrives
//!   silently, level 3 still rings; "🏠 我在家" under a camera alarm deletes
//!   that push everywhere; "🏷 标注" appears only with people or vehicles;
//! - an alarm that drops to level 1 is withdrawn (push and clip) — unless it
//!   rang the phone: that push stays, its text updated, for the feedback;
//! - a chat that sent `/cctv_off` gets none of this until `/cctv_on`, except
//!   the alarms that also ring the phone (confirmed intrusion, cameras down).

use super::alarm::{self, AlarmLine, ACK_PREFIX};
use super::api::{CctvApi, Event};
use super::format;
use super::home::{self, DISMISSED, HOME_PREFIX};
use super::label::TAG_PREFIX;
use super::store::{Pushed, Sent, Store};
use super::tg::{row, Bot, Buttons, SendOpts};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};
use chrono_tz::Tz;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant};

const WAIT_SECS: u64 = 25;
const DOWN_AFTER: Duration = Duration::from_secs(180);
const SUMMARY_PAGES: usize = 30;

pub struct Ctx {
    pub api: CctvApi,
    pub bot: Bot,
    pub chats: Vec<String>,
    pub tz: Tz,
    /// Minutes after midnight.
    pub quiet: (u32, u32),
    pub summary_at: u32,
    /// The phone line for the highest alarms (`None`: Telegram only).
    pub alarm: Option<Arc<AlarmLine>>,
    /// Where `state/cctv.db` lives (clip deliveries record their replies).
    pub workspace: std::path::PathBuf,
}

/// Event flag: the alarm had no people or vehicles, so no "标注" button.
pub(super) const NOTAG: &str = "notag";
/// Event flag: this alarm rang the phone — never withdrawn, no "我在家".
const CALLED: &str = "called";
/// Event flag: withdrawn after dropping to level 1.
const WITHDRAWN: &str = "withdrawn";

impl Ctx {
    fn now_minutes(&self, now: DateTime<Utc>) -> u32 {
        let l = now.with_timezone(&self.tz);
        l.hour() * 60 + l.minute()
    }

    pub fn is_quiet(&self, now: DateTime<Utc>) -> bool {
        let m = self.now_minutes(now);
        let (start, end) = self.quiet;
        if start <= end {
            (start..end).contains(&m)
        } else {
            m >= start || m < end
        }
    }
}

/// Does this event call for the phone (user's rule)?
pub fn needs_phone(e: &Event) -> bool {
    (e.kind == "alarm" && e.intrusion_confirmed)
        || (e.kind == "system"
            && e.cause.as_deref() == Some("Device offline")
            && e.offline_cameras.is_some_and(|n| n >= 2))
}

pub async fn run(ctx: Arc<Ctx>, store: Store) -> Result<()> {
    let mut cursor: Option<i64> = store.get("cursor")?.and_then(|c| c.parse().ok());
    let mut failing_since: Option<Instant> = None;
    let mut down_notice: Vec<Sent> = Vec::new();
    let mut last_prune = Instant::now();
    tracing::info!(?cursor, "CCTV worker started");
    loop {
        if let Err(e) = morning_summary(&ctx, &store).await {
            tracing::warn!("CCTV morning summary failed: {e:#}");
        }
        home::remind_if_expired(&ctx.bot, &store).await;
        if last_prune.elapsed() > Duration::from_secs(6 * 3600) {
            let _ = store.prune();
            last_prune = Instant::now();
        }
        match ctx.api.stream(cursor, WAIT_SECS).await {
            Ok(page) => {
                if let Some(since) = failing_since {
                    record(
                        "CCTV stream back",
                        serde_json::json!({"down_ms": since.elapsed().as_millis()}),
                    );
                }
                if failing_since.take().is_some() && !down_notice.is_empty() {
                    let text = "✅ 监控服务已恢复连接";
                    for s in down_notice.drain(..) {
                        let opts = SendOpts {
                            silent: true,
                            parent: Some(s.message_id),
                            buttons: Vec::new(),
                        };
                        let _ = ctx.bot.send_text(&s.chat, text, &opts).await;
                    }
                }
                let Some(c) = cursor else {
                    cursor = Some(page.cursor);
                    store.set("cursor", &page.cursor.to_string())?;
                    continue;
                };
                if page.gap {
                    broadcast(
                        &ctx,
                        &audience(&ctx, &store),
                        "⚠️ 监控事件有中断（elfClaw 离线太久），中间部分事件没有推送。",
                        true,
                    )
                    .await;
                }
                for e in page.events {
                    if let Err(err) = handle(&ctx, &store, e).await {
                        tracing::warn!("CCTV event not handled: {err:#}");
                    }
                }
                if page.cursor != c {
                    cursor = Some(page.cursor);
                    store.set("cursor", &page.cursor.to_string())?;
                }
            }
            Err(e) => {
                if failing_since.is_none() {
                    record(
                        "CCTV stream lost",
                        serde_json::json!({"error": format!("{e:#}")}),
                    );
                }
                let since = *failing_since.get_or_insert_with(Instant::now);
                tracing::warn!("CCTV stream failed: {e:#}");
                if since.elapsed() >= DOWN_AFTER && down_notice.is_empty() {
                    down_notice = monitoring_down(&ctx).await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

/// The chats that have not switched pushes off.
fn audience(ctx: &Ctx, store: &Store) -> Vec<String> {
    ctx.chats
        .iter()
        .filter(|c| !store.is_muted(c))
        .cloned()
        .collect()
}

/// Who gets an event: everyone for an alarm that rings the phone (muting
/// must not hide a confirmed intrusion), otherwise the unmuted chats.
fn recipients(ctx: &Ctx, store: &Store, e: &Event) -> Vec<String> {
    if needs_phone(e) {
        ctx.chats.clone()
    } else {
        audience(ctx, store)
    }
}

async fn broadcast(ctx: &Ctx, chats: &[String], text: &str, silent: bool) -> Vec<Sent> {
    let mut sent = Vec::new();
    for chat in chats {
        let opts = SendOpts {
            silent,
            parent: None,
            buttons: Vec::new(),
        };
        match ctx.bot.send_text(chat, text, &opts).await {
            Ok(id) => sent.push(Sent {
                chat: chat.clone(),
                message_id: id,
                is_photo: false,
                caption: text.to_string(),
            }),
            Err(e) => tracing::warn!("CCTV notice not delivered: {e:#}"),
        }
    }
    sent
}

/// cctv-server unreachable: every camera is effectively down — a highest alarm.
async fn monitoring_down(ctx: &Ctx) -> Vec<Sent> {
    let key = format!("down{}", Utc::now().timestamp());
    let data = format!("{ACK_PREFIX}{key}");
    let text = "🚨 监控服务连不上已超过 3 分钟（所有摄像头都看不到了）";
    let buttons = if ctx.alarm.is_some() {
        row(&[("✅ 已看到", data.as_str())])
    } else {
        Vec::new()
    };
    let mut sent = Vec::new();
    for chat in &ctx.chats {
        let opts = SendOpts {
            silent: false,
            parent: None,
            buttons: buttons.clone(),
        };
        match ctx.bot.send_text(chat, text, &opts).await {
            Ok(id) => sent.push(Sent {
                chat: chat.clone(),
                message_id: id,
                is_photo: false,
                caption: text.into(),
            }),
            Err(e) => tracing::warn!("CCTV down notice not delivered: {e:#}"),
        }
    }
    if let Some(line) = &ctx.alarm {
        alarm::start(
            line.clone(),
            key,
            "监控服务连不上，所有摄像头都看不到了".into(),
            sent.clone(),
        );
    }
    sent
}

async fn handle(ctx: &Ctx, store: &Store, e: Event) -> Result<()> {
    match e.kind.as_str() {
        "alarm" | "system" | "recovery" => handle_event(ctx, store, &e).await,
        "report" => handle_report(ctx, store, &e).await,
        "clip_ready" => {
            if let Some(job) = e.job_id.as_deref() {
                deliver_job_clip(ctx, store, job, &e);
            } else if let Some(id) = e.event_id {
                deliver_event_clip(ctx, store, id, &e)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// cctv's own summaries (v1.8): only the nightly report is pushed (like a
/// level 2 alarm). The 15-minute summaries are not — they repeated the same
/// parked car every quarter hour (owner's decision 2026-10-17); they stay
/// queryable through the `cctv` tool. Each report once.
async fn handle_report(ctx: &Ctx, store: &Store, e: &Event) -> Result<()> {
    let id = e.report_id.context("report without id")?;
    let daily = e.report_kind.as_deref() == Some("daily");
    if !daily || e.level < 2 || !store.first_report(id)? {
        return Ok(());
    }
    let silent = ctx.is_quiet(Utc::now());
    broadcast(ctx, &audience(ctx, store), &format::report(e), silent).await;
    Ok(())
}

async fn handle_event(ctx: &Ctx, store: &Store, e: &Event) -> Result<()> {
    let id = e.event_id.context("event without id")?;
    let key = format!("e{id}");
    // Level 2 arrives silently at night and while someone is home; level 3
    // always rings.
    let quiet = ctx.is_quiet(Utc::now()) || home::active(store);
    if store.has_flag(id, WITHDRAWN) {
        if e.level < 2 {
            return Ok(());
        }
        // Back up after being withdrawn: push it afresh.
        store.clear_flag(id, WITHDRAWN)?;
        store.forget_push(id)?;
    }
    let Some(mut p) = store.pushed(id)? else {
        if e.level < 2 {
            return Ok(()); // level 1: summary only
        }
        let phone = needs_phone(e) && ctx.alarm.is_some();
        if phone {
            store.set_flag(id, CALLED)?;
        }
        let silent = e.kind == "recovery" || (e.level == 2 && quiet);
        let caption = format::caption(e, false);
        let started = Instant::now();
        let taggable = e.kind == "alarm" && has_targets(ctx, id).await;
        if !taggable {
            store.set_flag(id, NOTAG)?;
        }
        let buttons = push_buttons(e, phone, &key, taggable, !phone);
        // All chats at once: one slow send must not hold up the others.
        let mut tasks = tokio::task::JoinSet::new();
        for chat in recipients(ctx, store, e) {
            let (bot, caption) = (ctx.bot.clone(), caption.clone());
            let opts = SendOpts {
                silent,
                parent: None,
                buttons: buttons.clone(),
            };
            tasks.spawn(async move {
                let t = Instant::now();
                let r = bot.send_text(&chat, &caption, &opts).await;
                (chat, r, t.elapsed().as_millis())
            });
        }
        let mut sent = Vec::new();
        let mut timings = Vec::new();
        let mut failures = Vec::new();
        while let Some(done) = tasks.join_next().await {
            let Ok((chat, r, ms)) = done else { continue };
            match r {
                Ok(message_id) => {
                    timings.push(serde_json::json!({"chat": chat, "ms": ms}));
                    let s = Sent {
                        chat,
                        message_id,
                        is_photo: false,
                        caption: caption.clone(),
                    };
                    store.save_message(id, &s)?;
                    sent.push(s);
                }
                Err(err) => {
                    timings.push(
                        serde_json::json!({"chat": chat, "ms": ms, "error": format!("{err:#}")}),
                    );
                    tracing::warn!(event = id, "CCTV push not delivered: {err:#}");
                    failures.push(format!("{err:#}"));
                }
            }
        }
        record(
            &format!("CCTV push e{id} (level {})", e.level),
            serde_json::json!({
                "event_id": id,
                "level": e.level,
                "sends": timings,
                "total_ms": started.elapsed().as_millis(),
            }),
        );
        store.set_pushed(
            id,
            Pushed {
                level: e.level,
                has_description: e.description.is_some(),
                ..Pushed::default()
            },
        )?;
        if !e.test && id > 0 {
            let report = super::api::delivery(
                "telegram",
                Some(sent.len()),
                !sent.is_empty(),
                &failures.join("；"),
            );
            if let Err(err) = ctx.api.delivered(id, &report).await {
                tracing::debug!(event = id, "delivery not reported: {err:#}");
            }
        }
        if phone {
            if let Some(line) = &ctx.alarm {
                alarm::start(line.clone(), key, format::headline(e), sent);
            }
        }
        return Ok(());
    };

    if store.has_flag(id, DISMISSED) && !needs_phone(e) {
        return Ok(()); // the owner deleted it ("我在家")
    }
    let messages = store.messages(id)?;
    if e.level > p.level && e.level >= 2 {
        // Danger rose: push again, as a reply to the first push.
        let phone = needs_phone(e) && ctx.alarm.is_some();
        if phone {
            store.set_flag(id, CALLED)?;
        }
        let text = format!("⬆️ 升级：{}", format::caption(e, false));
        let buttons = push_buttons(
            e,
            phone,
            &key,
            !store.has_flag(id, NOTAG),
            !store.has_flag(id, CALLED),
        );
        let mut sent = Vec::new();
        for chat in &recipients(ctx, store, e) {
            let parent = messages
                .iter()
                .find(|m| &m.chat == chat)
                .map(|m| m.message_id);
            let opts = SendOpts {
                silent: e.level == 2 && quiet,
                parent,
                buttons: buttons.clone(),
            };
            match ctx.bot.send_text(chat, &text, &opts).await {
                Ok(message_id) => {
                    store.save_reply(id, chat, message_id)?;
                    sent.push(Sent {
                        chat: chat.clone(),
                        message_id,
                        is_photo: false,
                        caption: text.clone(),
                    });
                }
                Err(err) => tracing::warn!(event = id, "CCTV upgrade not delivered: {err:#}"),
            }
        }
        p.level = e.level;
        p.marked_false = false;
        if phone && !alarm::is_pending(&key) {
            if let Some(line) = &ctx.alarm {
                alarm::start(line.clone(), key.clone(), format::headline(e), sent);
            }
        }
    } else if e.level <= 1 && p.level >= 2 {
        if !store.has_flag(id, CALLED) {
            // Owner's rule: an alarm found harmless after it was pushed is
            // taken back, push and clip.
            withdraw_push(&ctx.bot, store, id).await;
            store.set_flag(id, WITHDRAWN)?;
            record(
                &format!("CCTV push e{id} withdrawn (level {})", e.level),
                serde_json::json!({"event_id": id, "level": e.level}),
            );
            return Ok(());
        }
        p.marked_false = true;
    }
    p.has_description |= e.description.is_some();
    // An alarm the AI timed out on is pushed before its people and vehicles
    // are saved; they come with a later update — then "标注" is added.
    let tag_added = e.kind == "alarm"
        && store.has_flag(id, NOTAG)
        && has_targets(ctx, id).await
        && store.clear_flag(id, NOTAG).is_ok();
    let caption = format::caption(e, p.marked_false);
    let stale: Vec<Sent> = messages
        .into_iter()
        .filter(|m| tag_added || m.caption != caption)
        .collect();
    if !stale.is_empty() {
        edit_all(&ctx.bot, store, id, &stale, e, &caption, &key).await;
    }
    store.set_pushed(id, p)?;
    Ok(())
}

/// The buttons under a push: "已看到" while a phone alarm waits (`ack`:
/// its key), and on a camera alarm "标注" (when it has people or vehicles)
/// and "我在家".
pub(super) fn alarm_buttons(
    camera_event: Option<i64>,
    ack: Option<&str>,
    taggable: bool,
    home: bool,
) -> Buttons {
    let ack_data = ack.map(|k| format!("{ACK_PREFIX}{k}"));
    let tag_data = camera_event
        .filter(|_| taggable)
        .map(|id| format!("{TAG_PREFIX}{id}"));
    let home_data = camera_event
        .filter(|_| home)
        .map(|id| format!("{HOME_PREFIX}{id}"));
    let mut buttons = Vec::new();
    if let Some(d) = &ack_data {
        buttons.push(("✅ 已看到", d.as_str()));
    }
    if let Some(d) = &tag_data {
        buttons.push(("🏷 标注", d.as_str()));
    }
    if let Some(d) = &home_data {
        buttons.push(("🏠 我在家", d.as_str()));
    }
    row(&buttons)
}

/// `home`: offer "我在家" (not under a push that rang the phone).
fn push_buttons(e: &Event, ack: bool, key: &str, taggable: bool, home: bool) -> Buttons {
    let camera = e.event_id.filter(|id| *id > 0 && e.kind == "alarm");
    alarm_buttons(camera, ack.then_some(key), taggable, home)
}

/// Delete an alarm's push and its replies (clip, upgrade) in every chat.
pub(super) async fn withdraw_push(bot: &Bot, store: &Store, id: i64) {
    let mut targets: Vec<(String, i64)> = store
        .messages(id)
        .unwrap_or_default()
        .into_iter()
        .map(|m| (m.chat, m.message_id))
        .collect();
    targets.extend(store.replies(id).unwrap_or_default());
    for (chat, message) in targets {
        if let Err(e) = bot.delete(&chat, message).await {
            tracing::warn!(event = id, "alarm message not deleted: {e:#}");
        }
    }
}

/// No clip for an alarm the owner deleted, one that was withdrawn, or one
/// pushed before the last "我在家".
fn clip_unwanted(store: &Store, id: i64) -> bool {
    store.has_flag(id, DISMISSED)
        || store.has_flag(id, WITHDRAWN)
        || home::pressed_after_push(store, id)
}

/// Does the alarm have people or vehicles to label? cctv answering slowly
/// or not at all counts as yes: a useless button beats a missing one.
async fn has_targets(ctx: &Ctx, id: i64) -> bool {
    match tokio::time::timeout(Duration::from_millis(1500), ctx.api.objects(id)).await {
        Ok(Ok(objects)) => !objects.is_empty(),
        _ => true,
    }
}

/// Bring the pushes of event `id` up to `caption`, remembering what each
/// now shows so the next update edits only what changed.
async fn edit_all(
    bot: &Bot,
    store: &Store,
    id: i64,
    messages: &[Sent],
    e: &Event,
    caption: &str,
    key: &str,
) {
    let buttons = push_buttons(
        e,
        alarm::is_pending(key),
        key,
        !store.has_flag(id, NOTAG),
        !store.has_flag(id, CALLED),
    );
    for m in messages {
        match bot
            .edit(&m.chat, m.message_id, caption, m.is_photo, &buttons)
            .await
        {
            Ok(()) => {
                let updated = Sent {
                    caption: caption.to_string(),
                    ..m.clone()
                };
                if let Err(err) = store.save_message(id, &updated) {
                    tracing::warn!("CCTV caption not recorded: {err:#}");
                }
            }
            Err(err) => tracing::warn!("CCTV caption not updated: {err:#}"),
        }
    }
}

/// Bring a pushed event's messages up to its current state, outside the
/// event stream: since v1.12 a label that does not lower the level changes
/// the description without a stream update, so the labeller refreshes.
pub(super) async fn refresh_push(bot: &Bot, store: &Store, e: &Event) -> Result<()> {
    let Some(id) = e.event_id else {
        return Ok(());
    };
    let Some(p) = store.pushed(id)? else {
        return Ok(()); // never pushed
    };
    let caption = format::caption(e, p.marked_false);
    let stale: Vec<Sent> = store
        .messages(id)?
        .into_iter()
        .filter(|m| m.caption != caption)
        .collect();
    if !stale.is_empty() {
        edit_all(bot, store, id, &stale, e, &caption, &format!("e{id}")).await;
    }
    Ok(())
}

/// A line in the elfClaw run log (`state/elfclaw-logs.jsonl`), so push
/// delays and lost connections can be traced afterwards.
fn record(message: &str, details: serde_json::Value) {
    crate::elfclaw_log::log(crate::elfclaw_log::LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        level: crate::elfclaw_log::LogLevel::Info,
        category: crate::elfclaw_log::LogCategory::ChannelMessage,
        component: "cctv".into(),
        message: message.into(),
        details,
    });
}

fn deliver_event_clip(ctx: &Ctx, store: &Store, id: i64, e: &Event) -> Result<()> {
    let Some(mut p) = store.pushed(id)? else {
        return Ok(()); // never pushed (level 1, disarmed): no clip either
    };
    if p.clip_done || clip_unwanted(store, id) {
        return Ok(());
    }
    p.clip_done = true;
    store.set_pushed(id, p)?;
    let messages: Vec<Sent> = store
        .messages(id)?
        .into_iter()
        .filter(|m| !store.is_muted(&m.chat))
        .collect();
    let clip = e.clip.clone().unwrap_or_default();
    let (api, bot, workspace) = (ctx.api.clone(), ctx.bot.clone(), ctx.workspace.clone());
    tokio::spawn(async move {
        // Replies are recorded so "我在家" can delete them with the push;
        // a push dismissed while the clip downloaded gets none.
        let store = Store::open(&workspace).ok();
        let dismissed = || store.as_ref().is_some_and(|s| clip_unwanted(s, id));
        let remember = |chat: &str, sent: Result<i64>| match sent {
            Ok(message_id) => {
                if let Some(s) = &store {
                    let _ = s.save_reply(id, chat, message_id);
                }
            }
            Err(err) => tracing::warn!(event = id, "CCTV clip reply not delivered: {err:#}"),
        };
        let reply_opts = |m: &Sent| SendOpts {
            silent: true,
            parent: Some(m.message_id),
            buttons: Vec::new(),
        };
        if clip.status != "ready" {
            let reason = clip.reason.unwrap_or_else(|| "原因不明".into());
            for m in messages.iter().filter(|_| !dismissed()) {
                let r = bot
                    .send_text(&m.chat, &format!("这次没有录像：{reason}"), &reply_opts(m))
                    .await;
                remember(&m.chat, r);
            }
            return;
        }
        let Some(url) = clip.url.as_deref() else {
            return;
        };
        let Some(mp4) = fetch_clip(&api, url).await else {
            for m in messages.iter().filter(|_| !dismissed()) {
                let r = bot
                    .send_text(
                        &m.chat,
                        "录像取不下来，可以点上面的链接在网页上看",
                        &reply_opts(m),
                    )
                    .await;
                remember(&m.chat, r);
            }
            return;
        };
        let caption = format::clip_caption(clip.duration, None);
        for m in messages.iter().filter(|_| !dismissed()) {
            let r = bot
                .send_video(&m.chat, mp4.clone(), &caption, &reply_opts(m))
                .await;
            remember(&m.chat, r);
        }
    });
    Ok(())
}

/// A clip someone asked for in Telegram goes back to that chat.
fn deliver_job_clip(ctx: &Ctx, store: &Store, job: &str, e: &Event) {
    let chat = match store.take_job(job) {
        Ok(Some(c)) => c,
        _ => return,
    };
    let clip = e.clip.clone().unwrap_or_default();
    let requested = requested_secs(e.from.as_deref(), e.to.as_deref());
    let label = format!(
        "{} {}–{}",
        e.monitor_name.as_deref().unwrap_or("监控"),
        e.from.as_deref().map(format::clock).unwrap_or_default(),
        e.to.as_deref().map(format::clock).unwrap_or_default()
    );
    let (api, bot) = (ctx.api.clone(), ctx.bot.clone());
    tokio::spawn(async move {
        let opts = SendOpts {
            silent: false,
            parent: None,
            buttons: Vec::new(),
        };
        if clip.status != "ready" {
            let reason = clip.reason.unwrap_or_else(|| "原因不明".into());
            let _ = bot
                .send_text(&chat, &format!("{label} 的录像没有生成：{reason}"), &opts)
                .await;
            return;
        }
        let Some(mp4) = (match clip.url.as_deref() {
            Some(url) => fetch_clip(&api, url).await,
            None => None,
        }) else {
            let _ = bot
                .send_text(&chat, &format!("{label} 的录像取不下来"), &opts)
                .await;
            return;
        };
        let caption = format!(
            "{label}\n{}",
            format::clip_caption(clip.duration, requested)
        );
        if let Err(err) = bot.send_video(&chat, mp4, &caption, &opts).await {
            tracing::warn!("requested clip not delivered: {err:#}");
            let _ = bot
                .send_text(&chat, &format!("{label} 的录像发送失败：{err}"), &opts)
                .await;
        }
    });
}

fn requested_secs(from: Option<&str>, to: Option<&str>) -> Option<f64> {
    let parse = |s: &str| DateTime::parse_from_rfc3339(s).ok();
    let (a, b) = (parse(from?)?, parse(to?)?);
    #[allow(clippy::cast_precision_loss)]
    Some((b - a).num_seconds() as f64)
}

async fn fetch_clip(api: &CctvApi, url: &str) -> Option<Vec<u8>> {
    for _ in 0..12 {
        match api.bytes(url, Duration::from_secs(120)).await {
            Ok(Some(b)) => return Some(b),
            Ok(None) => tokio::time::sleep(Duration::from_secs(5)).await,
            Err(e) => {
                tracing::warn!("CCTV clip download failed: {e:#}");
                return None;
            }
        }
    }
    None
}

/// Once a day at `summary_at`: what happened since the last summary.
async fn morning_summary(ctx: &Ctx, store: &Store) -> Result<()> {
    let now = Utc::now();
    let local = now.with_timezone(&ctx.tz);
    let today = local.date_naive().to_string();
    if ctx.now_minutes(now) < ctx.summary_at
        || store.get("summary_day")?.as_deref() == Some(today.as_str())
    {
        return Ok(());
    }
    store.set("summary_day", &today)?;
    let from = (local - ChronoDuration::hours(24))
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    let to = local.format("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let enc = |s: &str| s.replace('+', "%2B");
    let mut before: Option<i64> = None;
    let mut by_monitor: std::collections::BTreeMap<String, u32> =
        std::collections::BTreeMap::default();
    let (mut level1, mut pushed, mut highest) = (0u32, 0u32, 0u32);
    for _ in 0..SUMMARY_PAGES {
        let mut q = format!("from={}&to={}&limit=200", enc(&from), enc(&to));
        if let Some(b) = before {
            let _ = write!(q, "&before={b}");
        }
        let page = ctx.api.events(&q).await?;
        for e in &page.events {
            if e.kind != "alarm" {
                continue;
            }
            if e.level >= 2 {
                pushed += 1;
                if e.level >= 3 {
                    highest += 1;
                }
            } else {
                level1 += 1;
                *by_monitor
                    .entry(e.monitor_name.clone().unwrap_or_default())
                    .or_default() += 1;
            }
        }
        match page.next_before {
            Some(b) if !page.events.is_empty() => before = Some(b),
            _ => break,
        }
    }
    let mut text = format!("📋 监控汇总（过去 24 小时）\n• 推送过的报警：{pushed} 条（其中最高级 {highest} 条）\n• 只记录、没推送的移动侦测：{level1} 条");
    if !by_monitor.is_empty() {
        let mut parts: Vec<(String, u32)> = by_monitor.into_iter().collect();
        parts.sort_by(|a, b| b.1.cmp(&a.1));
        let list: Vec<String> = parts.iter().map(|(m, n)| format!("{m} {n}")).collect();
        let _ = write!(text, "\n  {}", list.join("、"));
    }
    broadcast(ctx, &audience(ctx, store), &text, true).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(quiet: (u32, u32)) -> Ctx {
        Ctx {
            api: CctvApi::for_tests(),
            bot: Bot::new("t"),
            chats: vec![],
            tz: chrono_tz::Australia::Sydney,
            quiet,
            summary_at: 7 * 60,
            alarm: None,
            workspace: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn camera_alarms_get_tag_only_with_targets_and_always_home() {
        let labels = |b: &Buttons| {
            b.iter()
                .flatten()
                .map(|(l, _)| l.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&alarm_buttons(Some(7), Some("e7"), true, false)),
            ["✅ 已看到", "🏷 标注"],
            "a push that rang the phone has no 我在家"
        );
        assert_eq!(
            labels(&alarm_buttons(Some(7), None, false, true)),
            ["🏠 我在家"]
        );
        assert!(
            alarm_buttons(None, None, true, true).is_empty(),
            "system events: none"
        );
        let b = alarm_buttons(Some(7), None, true, true);
        assert_eq!(b[0][1].1, "cctv_home:7");
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let c = ctx((23 * 60, 6 * 60 + 30));
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert!(c.is_quiet(at("2026-10-14T23:30:00+11:00")));
        assert!(c.is_quiet(at("2026-10-15T03:00:00+11:00")));
        assert!(!c.is_quiet(at("2026-10-15T06:30:00+11:00")));
        assert!(!c.is_quiet(at("2026-10-14T14:00:00+11:00")));
    }

    #[test]
    fn only_confirmed_intrusions_and_multi_camera_outages_ring_the_phone() {
        let alarm = Event {
            kind: "alarm".into(),
            level: 3,
            intrusion_confirmed: true,
            ..Event::default()
        };
        assert!(needs_phone(&alarm));
        assert!(!needs_phone(&Event {
            intrusion_confirmed: false,
            ..alarm.clone()
        }));
        let off = Event {
            kind: "system".into(),
            level: 3,
            cause: Some("Device offline".into()),
            offline_cameras: Some(2),
            ..Event::default()
        };
        assert!(needs_phone(&off));
        assert!(!needs_phone(&Event {
            offline_cameras: Some(1),
            ..off.clone()
        }));
        assert!(!needs_phone(&Event {
            cause: Some("Disk error".into()),
            ..off
        }));
        assert_eq!(
            requested_secs(
                Some("2026-10-14T14:00:00+11:00"),
                Some("2026-10-14T14:02:00+11:00")
            ),
            Some(120.0)
        );
    }
}
