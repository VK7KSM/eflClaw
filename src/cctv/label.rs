//! Telling cctv's AI who someone is (or whose car), from Telegram (contract
//! v1.5, section 10.5). Under each camera alarm push sits a "🏷 标注" button:
//!
//! 1. pressed → every person and vehicle of that alarm comes back as its own
//!    picture, replying to the push, with the registered names as buttons
//!    (people for a person, vehicles for a car);
//! 2. a name pressed → `POST /events/{id}/label`; the picture's caption
//!    becomes cctv's note (verbatim, as agreed) with a "撤销" button, or its
//!    reason when the target cannot be learnt;
//! 3. a new name: the owner replies to the picture ("这是小明"); its caption
//!    names the event and target, and the agent calls the `cctv` tool.
//!
//! Since v1.6 a person without a clear face can be labelled too: cctv then
//! remembers only that day's build and clothes (its note says so).

use super::api::{CctvApi, LabelOutcome, Name, Object};
use super::tg::{Bot, Buttons, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Callback data prefixes: the push's button, a name, an undo.
pub const TAG_PREFIX: &str = "cctv_tag:";
const NAME_PREFIX: &str = "cctv_lbl:";
const UNDO_PREFIX: &str = "cctv_unl:";
/// Pictures offered per press (a street full of parked cars is not useful).
const MAX_OFFERED: usize = 8;
const MAX_NAME_BUTTONS: usize = 12;

struct Labeler {
    api: CctvApi,
    bot: Bot,
}

static LABELER: OnceLock<Labeler> = OnceLock::new();

/// Name buttons: callback data is limited to 64 bytes and a name can be 32
/// Chinese characters, so the button carries a number standing for
/// (event, target, name). Lost on restart; the press then says so.
/// (event, target, name)
type Choice = (i64, u32, String);

static CHOICES: Mutex<Option<HashMap<u64, Choice>>> = Mutex::new(None);
static NEXT_CHOICE: AtomicU64 = AtomicU64::new(1);

/// Set up the button handlers (cctv link start-up).
pub fn register(api: CctvApi, bot: Bot) {
    if LABELER.set(Labeler { api, bot }).is_err() {
        return;
    }
    for (prefix, handler) in [
        (
            TAG_PREFIX,
            on_tag as fn(&CallbackPress<'_>) -> Option<CallbackAnswer>,
        ),
        (NAME_PREFIX, on_name),
        (UNDO_PREFIX, on_undo),
    ] {
        crate::channels::telegram::register_callback_handler(prefix, Arc::new(handler));
    }
}

fn answer(toast: &str) -> Option<CallbackAnswer> {
    Some(CallbackAnswer {
        toast: toast.to_string(),
        clear_buttons: false,
    })
}

fn on_tag(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let id: i64 = p.data.strip_prefix(TAG_PREFIX)?.parse().ok()?;
    let (chat, message) = (p.chat_id?.to_string(), p.message_id?);
    let l = LABELER.get()?;
    tokio::spawn(async move {
        if let Err(e) = offer(l, id, &chat, message).await {
            let opts = SendOpts {
                silent: true,
                parent: Some(message),
                buttons: Vec::new(),
            };
            let _ = l
                .bot
                .send_text(&chat, &format!("取不到这条报警里的人和车：{e:#}"), &opts)
                .await;
        }
    });
    answer("正在取这条报警里的人和车…")
}

fn on_name(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let token: u64 = p.data.strip_prefix(NAME_PREFIX)?.parse().ok()?;
    let Some((id, idx, name)) = CHOICES.lock().as_ref().and_then(|m| m.get(&token).cloned()) else {
        return answer("按钮已过期，请在报警下面重新点「标注」");
    };
    let (chat, message) = (p.chat_id?.to_string(), p.message_id?);
    let l = LABELER.get()?;
    let toast = format!("正在标成「{name}」…");
    tokio::spawn(async move {
        let (caption, buttons) = match l.api.label(id, &serde_json::json!(idx), &name, None).await {
            Ok(outcome) => labelled(id, idx, &outcome),
            Err(e) => (
                format!("事件 {id} · 目标 {idx}：标注失败（{e:#}），可以再点一次"),
                None,
            ),
        };
        let buttons = buttons.unwrap_or_default();
        if let Err(e) = l.bot.edit(&chat, message, &caption, true, &buttons).await {
            tracing::warn!("label result not shown: {e:#}");
        }
    });
    answer(&toast)
}

fn on_undo(p: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let (id, idx) = p.data.strip_prefix(UNDO_PREFIX)?.split_once(':')?;
    let (id, idx): (i64, u32) = (id.parse().ok()?, idx.parse().ok()?);
    let (chat, message) = (p.chat_id?.to_string(), p.message_id?);
    let l = LABELER.get()?;
    tokio::spawn(async move {
        let caption = match l.api.unlabel(id, idx).await {
            Ok(()) => format!("事件 {id} · 目标 {idx}：已撤销标注"),
            Err(e) => format!("事件 {id} · 目标 {idx}：撤销失败（{e:#}）"),
        };
        if let Err(e) = l
            .bot
            .edit(&chat, message, &caption, true, &Vec::new())
            .await
        {
            tracing::warn!("undo result not shown: {e:#}");
        }
    });
    answer("正在撤销…")
}

/// The caption (and undo button) after a label request.
fn labelled(id: i64, idx: u32, outcome: &LabelOutcome) -> (String, Option<Buttons>) {
    match outcome {
        LabelOutcome::Done { note, level } => {
            let mut text = format!("✅ {note}");
            if let Some((old, new)) = level.filter(|(old, new)| old != new) {
                let _ = write!(text, "\n这条报警从 level {old} 改为 level {new}");
            }
            let undo = format!("{UNDO_PREFIX}{id}:{idx}");
            (text, Some(super::tg::row(&[("↩ 撤销", undo.as_str())])))
        }
        LabelOutcome::Unlearnable(why) => (format!("⚠️ 学不了：{why}"), None),
    }
}

/// One line about a target, also used by the agent's tool.
pub fn describe(o: &Object) -> String {
    let what = if o.kind == "vehicle" { "车" } else { "人" };
    let status = match (o.kind.as_str(), o.status.as_str()) {
        (_, "known") => format!("认出是「{}」", o.matched.as_deref().unwrap_or("?")),
        ("person", "stranger") => "陌生人（看清了脸）".into(),
        ("person", "faceless") => "一直没看清脸".into(),
        ("person", "companion") => "家人的同伴".into(),
        ("person", "outside") => "在院子外".into(),
        ("vehicle", "moving") => "在院子里动".into(),
        ("vehicle", "parked") => "停着".into(),
        (_, other) => other.to_string(),
    };
    let mut line = format!("目标 {}：{what}，{status}", o.idx);
    if o.kind == "person" && o.has_face {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let px = o.face_px.max(0.0).round() as u32;
        let _ = write!(line, "，脸 {px} 像素");
    }
    if let Some(p) = o.plate.as_deref().filter(|p| !p.is_empty()) {
        let _ = write!(line, "，车牌 {p}");
    }
    if let Some(l) = &o.label {
        let _ = write!(line, "，已标为「{l}」");
    }
    if let Some(c) = o
        .category_zh
        .as_deref()
        .filter(|_| o.label.is_some() || o.matched.is_some())
    {
        let _ = write!(line, "（{c}）");
    }
    line
}

/// Name buttons for one target, two per row, each showing its category.
fn name_buttons(id: i64, idx: u32, names: &[Name]) -> Buttons {
    let mut guard = CHOICES.lock();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.len() > 2000 {
        map.clear();
    }
    let buttons: Vec<(String, String)> = names
        .iter()
        .take(MAX_NAME_BUTTONS)
        .map(|n| {
            let token = NEXT_CHOICE.fetch_add(1, Ordering::Relaxed);
            map.insert(token, (id, idx, n.name.clone()));
            let label = match &n.category_zh {
                Some(c) => format!("{}（{c}）", n.name),
                None => n.name.clone(),
            };
            (label, format!("{NAME_PREFIX}{token}"))
        })
        .collect();
    buttons.chunks(2).map(<[_]>::to_vec).collect()
}

async fn offer(l: &Labeler, id: i64, chat: &str, push: i64) -> Result<()> {
    let objects = l.api.objects(id).await?;
    let reply = |text: String| async move {
        let opts = SendOpts {
            silent: true,
            parent: Some(push),
            buttons: Vec::new(),
        };
        l.bot.send_text(chat, &text, &opts).await.map(|_| ())
    };
    if objects.is_empty() {
        return reply("这条报警没有保存人和车（AI 改版前的报警，或者已经超过 7 天）。".to_string())
            .await;
    }
    let people = l.api.people().await.unwrap_or_default();
    let vehicles = l.api.vehicles().await.unwrap_or_default();
    for o in objects.iter().take(MAX_OFFERED) {
        let jpeg = l
            .api
            .bytes(&o.crop_url, Duration::from_secs(20))
            .await?
            .context("截图还没准备好")?;
        let names = if o.kind == "vehicle" {
            &vehicles
        } else {
            &people
        };
        let hint = match (o.kind.as_str(), o.has_face) {
            ("vehicle", _) => "点名字标注；新的车请回复这张图，写上名字和分类，例如「这是邻居的车」",
            (_, true) => "点名字标注；新的人请回复这张图，写上名字和分类，例如「这是邻居小明」",
            (_, false) => {
                "没看清脸：标注后只按身形衣着认，今天之内有效。点名字标注；新的人请回复这张图，写上名字和分类，例如「这是邻居小明」"
            }
        };
        let caption = format!("事件 {id} · {}\n{hint}", describe(o));
        let opts = SendOpts {
            silent: true,
            parent: Some(push),
            buttons: name_buttons(id, o.idx, names),
        };
        l.bot.send_photo(chat, jpeg, &caption, &opts).await?;
    }
    if objects.len() > MAX_OFFERED {
        reply(format!(
            "还有 {} 个目标没有列出（只列前 {MAX_OFFERED} 个）。",
            objects.len() - MAX_OFFERED
        ))
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(kind: &str, status: &str, has_face: bool) -> Object {
        Object {
            idx: 0,
            kind: kind.into(),
            status: status.into(),
            has_face,
            face_px: if has_face { 64.4 } else { 0.0 },
            matched: None,
            label: None,
            category_zh: None,
            plate: None,
            crop_url: "/cctv/v1/events/1/objects/0.jpg".into(),
        }
    }

    #[test]
    fn targets_are_described_plainly() {
        let mut plate = object("vehicle", "moving", false);
        plate.plate = Some("ZZZ000".into());
        assert_eq!(describe(&plate), "目标 0：车，在院子里动，车牌 ZZZ000");
        assert_eq!(
            describe(&object("person", "faceless", false)),
            "目标 0：人，一直没看清脸"
        );
        assert_eq!(
            describe(&object("person", "stranger", true)),
            "目标 0：人，陌生人（看清了脸），脸 64 像素"
        );
        let mut car = object("vehicle", "known", false);
        car.matched = Some("我的车".into());
        car.label = Some("我的车".into());
        car.category_zh = Some("自家车".into());
        assert_eq!(
            describe(&car),
            "目标 0：车，认出是「我的车」，已标为「我的车」（自家车）"
        );
    }

    #[test]
    fn name_buttons_fit_telegram_and_map_back() {
        let names: Vec<Name> = [
            ("kasim", Some("家人")),
            ("一个很长很长很长很长很长很长很长很长很长的名字", None),
            ("c", Some("邻居")),
        ]
        .iter()
        .map(|(n, c)| Name {
            name: (*n).to_string(),
            category_zh: c.map(str::to_string),
        })
        .collect();
        let rows = name_buttons(36628, 3, &names);
        assert_eq!(rows.len(), 2, "two per row");
        assert_eq!(
            rows[0][0].0, "kasim（家人）",
            "the category shows on the button"
        );
        for (label, data) in rows.iter().flatten() {
            assert!(data.len() <= 64, "callback data limit");
            let token: u64 = data.strip_prefix(NAME_PREFIX).unwrap().parse().unwrap();
            let (id, idx, name) = CHOICES.lock().as_ref().unwrap()[&token].clone();
            assert_eq!((id, idx), (36628, 3));
            assert!(label.starts_with(&name), "the button sends its own name");
        }
    }

    #[test]
    fn label_results_read_as_agreed() {
        let (text, buttons) = labelled(
            7,
            1,
            &LabelOutcome::Done {
                note: "已把1个目标标成「kasim」".into(),
                level: Some((3, 1)),
            },
        );
        assert_eq!(
            text,
            "✅ 已把1个目标标成「kasim」\n这条报警从 level 3 改为 level 1"
        );
        assert_eq!(buttons.unwrap()[0][0].1, "cctv_unl:7:1");
        let (text, buttons) = labelled(7, 1, &LabelOutcome::Unlearnable("脸太小".into()));
        assert_eq!(text, "⚠️ 学不了：脸太小");
        assert!(buttons.is_none());
    }
}
