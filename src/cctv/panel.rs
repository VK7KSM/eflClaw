//! The monitoring panel (owner's design 2026-10-18): a reply keyboard that
//! stays under the chat with the elfClaw bot. Its buttons are answered by
//! the program, never by the model:
//!
//! | 📊 状态 | 🛡 布防 | 🔔 推送 |
//! | 📷 Garage front | 📷 Front yard fix | 📷 Front yard |
//! | 📷 Garage Back | 📷 Back yard | 📷 其它摄像头 |
//! | 🏠 我在家 | 🚪 我出门 | 📡 天线 |
//!
//! - a camera button sends its live picture, then what is in it, with
//!   "录 15 秒" and "再看一次" under it; "其它摄像头" lists the rest;
//! - 布防 / 推送 / 天线 show the state with buttons to change it; stopping
//!   the arming asks once more (owner's rule); 推送 switches only the
//!   presser's own account (owner's decision).

use super::api::{account_of, CctvApi, Monitor};
use super::format::{clock, rotator_line};
use super::look::{describe, Looker};
use super::store::Store;
use super::tg::{Bot, Buttons, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use anyhow::{Context, Result};
use chrono::Utc;
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const STATUS: &str = "📊 状态";
const ARM: &str = "🛡 布防";
const PUSH: &str = "🔔 推送";
const OTHER: &str = "📷 其它摄像头";
const HOME: &str = "🏠 我在家";
const AWAY: &str = "🚪 我出门";
const ANTENNA: &str = "📡 天线";
/// The cameras with a button of their own (owner's choice).
const KEY_CAMERAS: [&str; 5] = [
    "Garage front",
    "Front yard fix",
    "Front yard",
    "Garage Back",
    "Back yard",
];
/// Callback data prefix of the panel's inline buttons.
const P: &str = "cctv_p:";
/// Shown once to each account whenever this changes.
const PANEL_VERSION: &str = "2026-10-18";

struct Panel {
    api: CctvApi,
    bot: Bot,
    workspace: PathBuf,
    tz: Tz,
    send_to: Vec<String>,
    looker: Option<Looker>,
}

static PANEL: OnceLock<Panel> = OnceLock::new();

fn cam_button(name: &str) -> String {
    format!("📷 {name}")
}

/// The keyboard, row by row.
pub fn keyboard() -> Vec<Vec<String>> {
    let cams: Vec<String> = KEY_CAMERAS.iter().map(|c| cam_button(c)).collect();
    vec![
        vec![STATUS.into(), ARM.into(), PUSH.into()],
        cams[..3].to_vec(),
        vec![cams[3].clone(), cams[4].clone(), OTHER.into()],
        vec![HOME.into(), AWAY.into(), ANTENNA.into()],
    ]
}

/// The buttons' texts and `/cctv_panel` (daemon start-up, before Telegram
/// starts: the menu goes to Telegram then).
pub fn register_buttons(config: &crate::config::Config) {
    if !config.cctv.enabled {
        return;
    }
    let send_to = config.cctv.send_to.clone();
    for text in keyboard().into_iter().flatten() {
        let send_to = send_to.clone();
        let label = text.clone();
        crate::channels::telegram::register_text_command(
            &text,
            Arc::new(move |chat, sender| press(&label, chat, sender, &send_to)),
        );
    }
    let ours = send_to.clone();
    crate::channels::telegram::register_menu_command(
        "cctv_panel",
        "显示监控面板",
        Arc::new(move |chat, _sender| {
            if !ours.iter().any(|c| c.trim() == chat) {
                return "这个对话不接收监控推送。".into();
            }
            let Some(p) = PANEL.get() else {
                return "监控还没连上，稍后再试".into();
            };
            let chat = chat.to_string();
            tokio::spawn(async move {
                if let Err(e) = p.bot.send_keyboard(&chat, "📺 监控面板", &keyboard()).await {
                    tracing::warn!("panel not shown: {e:#}");
                }
            });
            String::new()
        }),
    );
}

/// Set up the inline buttons; show the panel once to each account when it
/// changed (cctv link start-up).
pub fn init(
    api: CctvApi,
    bot: Bot,
    workspace: PathBuf,
    tz: Tz,
    send_to: Vec<String>,
    looker: Option<Looker>,
) {
    let panel = Panel {
        api,
        bot,
        workspace,
        tz,
        send_to,
        looker,
    };
    if PANEL.set(panel).is_err() {
        return;
    }
    crate::channels::telegram::register_callback_handler(P, Arc::new(on_press));
    let Some(p) = PANEL.get() else { return };
    tokio::spawn(async move {
        let Ok(store) = Store::open(&p.workspace) else {
            return;
        };
        for chat in &p.send_to {
            let key = format!("panel_shown:{chat}");
            if store.get(&key).ok().flatten().as_deref() == Some(PANEL_VERSION) {
                continue;
            }
            let text = "📺 监控面板已更新：下面的按钮由程序直接处理。收起后点输入框旁的键盘图标就能再打开，菜单里 /cctv_panel 也可以。";
            match p.bot.send_keyboard(chat, text, &keyboard()).await {
                Ok(_) => {
                    let _ = store.set(&key, PANEL_VERSION);
                }
                Err(e) => tracing::warn!("panel not shown: {e:#}"),
            }
        }
    });
}

/// A button of the keyboard pressed. Answers by its own messages (the
/// returned text is sent only when not empty).
fn press(label: &str, chat: &str, sender: &str, send_to: &[String]) -> String {
    if !send_to.iter().any(|c| c.trim() == chat) {
        return "这个对话不接收监控推送。".into();
    }
    match label {
        HOME => return super::home::menu_home(chat),
        AWAY => return super::home::menu_away(chat, sender),
        _ => {}
    }
    let Some(p) = PANEL.get() else {
        return "监控还没连上，稍后再试".into();
    };
    let (chat, label) = (chat.to_string(), label.to_string());
    tokio::spawn(async move {
        let result = match label.as_str() {
            STATUS => show_status(p, &chat).await,
            ARM => show_arm(p, &chat).await,
            PUSH => show_push(p, &chat).await,
            OTHER => show_other(p, &chat).await,
            ANTENNA => show_antenna(p, &chat).await,
            cam => match cam.strip_prefix("📷 ") {
                Some(name) => snap_by_name(p, &chat, name).await,
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            let _ = p
                .bot
                .send_text(&chat, &format!("出错了：{e:#}"), &quiet(Vec::new()))
                .await;
        }
    });
    String::new()
}

fn quiet(buttons: Buttons) -> SendOpts {
    SendOpts {
        silent: true,
        parent: None,
        buttons,
    }
}

fn data(rest: &str) -> String {
    format!("{P}{rest}")
}

// ── 状态 ────────────────────────────────────────────────────────────────

async fn show_status(p: &Panel, chat: &str) -> Result<()> {
    let v = p.api.status().await?;
    p.bot
        .send_text(chat, &status_text(&v), &quiet(Vec::new()))
        .await?;
    Ok(())
}

fn hhmm(v: &Value) -> String {
    v.as_str()
        .map(|t| clock(t).chars().take(5).collect())
        .unwrap_or_default()
}

fn names(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["name"].as_str())
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default()
}

/// The status message (contract v1.14 §13).
fn status_text(v: &Value) -> String {
    let mut out = format!("📊 监控状态 {}", hhmm(&v["time"]));
    let pr = &v["presence"];
    let home = if pr["home"].as_bool() == Some(true) {
        format!(
            "🏠 有人在家（{} 按的，到 {}）",
            pr["account"].as_str().unwrap_or("?"),
            hhmm(&pr["until"])
        )
    } else if pr["away_since"].is_string() {
        format!("🚪 没人在家（{} 按了出门）", hhmm(&pr["away_since"]))
    } else {
        "🚪 没人在家".into()
    };
    let _ = write!(out, "\n{home}");
    if let Some(s) = v["arm"]["summary"].as_str() {
        let _ = write!(out, "\n🛡 {s}");
    }
    let cc = &v["camera_count"];
    let _ = write!(out, "\n📷 摄像头 {}/{} 在线", cc["online"], cc["total"]);
    let offline = names(&cc["offline"]);
    if !offline.is_empty() {
        let _ = write!(out, "；连不上：{offline}");
    }
    let unpowered = names(&cc["unpowered"]);
    if !unpowered.is_empty() {
        let _ = write!(out, "；没通电：{unpowered}");
    }
    let ai = &v["ai"];
    if ai["online"].as_bool() == Some(true) {
        let _ = write!(out, "\n🤖 本地 AI 在线");
        if let Some(ms) = ai["frame_ms"].as_f64() {
            let _ = write!(out, "，每帧 {ms:.0} 毫秒");
        }
        if let Some(n) = ai["analyzing"].as_u64().filter(|n| *n > 0) {
            let _ = write!(out, "，正在细看 {n} 路");
        }
    } else {
        out.push_str("\n🤖 本地 AI 不在线");
    }
    let push = &v["push"];
    let _ = write!(
        out,
        "\n🔔 elfClaw {}，ntfy {}",
        if push["elfclaw"].as_bool() == Some(true) {
            "连着"
        } else {
            "没连上"
        },
        if push["ntfy"].as_bool() == Some(true) {
            "开"
        } else {
            "关"
        }
    );
    let nvr = &v["nvr"];
    if nvr["online"].as_bool() == Some(true) {
        let _ = write!(out, "\n💽 NVR 正常");
        if let Some(d) = nvr["days"].as_u64() {
            let _ = write!(out, "，可回看 {d} 天");
        }
        if nvr["disk_ok"].as_bool() == Some(false) {
            out.push_str("，硬盘有问题");
        }
        if nvr["recording_ok"].as_bool() == Some(false) {
            out.push_str("，有录像中断");
        }
    } else if nvr["configured"].as_bool() == Some(true) {
        out.push_str("\n💽 NVR 连不上");
    }
    let light = &v["light"];
    if light.is_object() {
        let _ = write!(
            out,
            "\n🌗 现在{}，日出 {}，日落 {}",
            if light["night"].as_bool() == Some(true) {
                "是夜里"
            } else {
                "是白天"
            },
            hhmm(&light["sunrise"]),
            hhmm(&light["sunset"])
        );
    }
    if let Some(f) = v["faults"].as_array().filter(|f| !f.is_empty()) {
        let list: Vec<&str> = f.iter().filter_map(Value::as_str).collect();
        let _ = write!(out, "\n⚠️ {}", list.join("；"));
    }
    let t = &v["today"];
    if t.is_object() {
        let _ = write!(
            out,
            "\n📈 今天报警 {} 条（打电话级 {}、推送级 {}、只记录 {}），送达 {}、打电话 {}",
            t["alarms"], t["level3"], t["level2"], t["level1"], t["delivered"], t["calls"]
        );
        let _ = write!(
            out,
            "\n　 认出家人 {}、熟人 {}、陌生人进院子 {}、路人 {}；自家车 {}、院子里车在动 {}、车牌 {} 个",
            t["family"],
            t["acquainted"],
            t["strangers"],
            t["passers"],
            t["own_vehicle"],
            t["moving_vehicles"],
            t["plates"]
        );
    }
    let la = &v["last_alarm"];
    if la.is_object() {
        let desc: String = la["description"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
        let _ = write!(
            out,
            "\n🚨 最近一条推送级报警：{} {} {}",
            hhmm(&la["time"]),
            la["monitor_name"].as_str().unwrap_or(""),
            la["cause_zh"].as_str().unwrap_or("")
        );
        if !desc.is_empty() {
            let _ = write!(out, "：{desc}");
        }
    }
    let fb = &v["call_feedback"]["recent"];
    if fb["calls"].as_u64().is_some_and(|n| n > 0) {
        let _ = write!(
            out,
            "\n☎️ 今天电话 {} 次：有用 {}、没用 {}、没反馈 {}",
            fb["calls"], fb["useful"], fb["useless"], fb["no_feedback"]
        );
    }
    let cams: Vec<String> = v["cameras"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|c| c["last"].is_object())
                .map(|c| {
                    format!(
                        "· {}：{} {}（今天 {} 条）",
                        c["name"].as_str().unwrap_or("?"),
                        hhmm(&c["last"]["time"]),
                        c["last"]["text"].as_str().unwrap_or(""),
                        c["today"]["alarms"]
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if !cams.is_empty() {
        let _ = write!(out, "\n各路最近一次：\n{}", cams.join("\n"));
    }
    out
}

// ── 布防 ────────────────────────────────────────────────────────────────

fn arm_text(v: &Value) -> String {
    let mut out = format!(
        "🛡 {}",
        v["arm"]["summary"].as_str().unwrap_or("布防状态不明")
    );
    if let Some(cams) = v["cameras"].as_array() {
        for c in cams {
            let state = match c["arm"].as_str() {
                Some("armed") => "布防".to_string(),
                Some(_) => match c["arm_until"].as_str() {
                    Some(u) => format!("撤防到 {}", clock(u).chars().take(5).collect::<String>()),
                    None => "撤防".into(),
                },
                None => continue,
            };
            let _ = write!(out, "\n· {}：{state}", c["name"].as_str().unwrap_or("?"));
        }
    }
    out
}

fn arm_buttons() -> Buttons {
    vec![vec![
        ("🛡 全部布防".into(), data("arm:on")),
        ("⏸ 全部撤防".into(), data("arm:off")),
    ]]
}

async fn show_arm(p: &Panel, chat: &str) -> Result<()> {
    let v = p.api.status().await?;
    p.bot
        .send_text(chat, &arm_text(&v), &quiet(arm_buttons()))
        .await?;
    Ok(())
}

// ── 推送 ────────────────────────────────────────────────────────────────

fn push_text(p: &Panel, chat: &str) -> String {
    let store = Store::open(&p.workspace).ok();
    let state = |c: &str| {
        if store.as_ref().is_some_and(|s| s.is_muted(c)) {
            "已停"
        } else {
            "开着"
        }
    };
    let mut out = format!("🔔 推送开关只管你自己这个账号。\n你：{}", state(chat));
    for other in p.send_to.iter().filter(|c| c.trim() != chat) {
        let _ = write!(out, "\n另一个账号：{}", state(other.trim()));
    }
    out.push_str("\n（会打电话的最高级报警，停了推送也照样推。）");
    out
}

fn push_buttons() -> Buttons {
    vec![vec![
        ("🔔 启动推送".into(), data("push:on")),
        ("🔕 停止推送".into(), data("push:off")),
    ]]
}

async fn show_push(p: &Panel, chat: &str) -> Result<()> {
    p.bot
        .send_text(chat, &push_text(p, chat), &quiet(push_buttons()))
        .await?;
    Ok(())
}

// ── 摄像头 ──────────────────────────────────────────────────────────────

fn cam_buttons(id: u32) -> Buttons {
    vec![vec![
        ("🎬 录 15 秒".into(), data(&format!("clip:{id}"))),
        ("🔄 再看一次".into(), data(&format!("snap:{id}"))),
    ]]
}

fn unavailable(m: &Monitor) -> Option<&'static str> {
    match (m.online, m.powered) {
        (true, _) => None,
        (false, Some(false)) => Some("没通电（不是故障）"),
        (false, _) => Some("连不上"),
    }
}

async fn snap_by_name(p: &Panel, chat: &str, name: &str) -> Result<()> {
    let monitors = p.api.monitors().await?;
    let m = monitors
        .iter()
        .find(|m| m.name == name)
        .with_context(|| format!("没有摄像头「{name}」"))?;
    snap(p, chat, m).await
}

async fn snap_by_id(p: &Panel, chat: &str, id: u32) -> Result<()> {
    let monitors = p.api.monitors().await?;
    let m = monitors
        .iter()
        .find(|m| m.id == id)
        .with_context(|| format!("没有第 {id} 路摄像头"))?;
    snap(p, chat, m).await
}

/// The live picture, then (by editing its caption) what is in it.
async fn snap(p: &Panel, chat: &str, m: &Monitor) -> Result<()> {
    if let Some(why) = unavailable(m) {
        p.bot
            .send_text(chat, &format!("{} 现在{why}", m.name), &quiet(Vec::new()))
            .await?;
        return Ok(());
    }
    let jpeg = p.api.live_snapshot(m.id).await?;
    let at = Utc::now()
        .with_timezone(&p.tz)
        .format("%H:%M:%S")
        .to_string();
    let head = format!("📷 {} {at}", m.name);
    let message = p
        .bot
        .send_photo(
            chat,
            jpeg.clone(),
            &format!("{head}\n正在看画面…"),
            &quiet(cam_buttons(m.id)),
        )
        .await?;
    let what = format!("摄像头 {} 在 {at} 的实时画面", m.name);
    let seen = describe(p.looker.as_ref(), &jpeg, &what, None).await;
    let seen: String = seen.chars().take(900).collect();
    p.bot
        .edit(
            chat,
            message,
            &format!("{head}\n{seen}"),
            true,
            &cam_buttons(m.id),
        )
        .await
}

async fn show_other(p: &Panel, chat: &str) -> Result<()> {
    let monitors = p.api.monitors().await?;
    let buttons: Vec<(String, String)> = monitors
        .iter()
        .filter(|m| !KEY_CAMERAS.contains(&m.name.as_str()))
        .map(|m| {
            let label = match unavailable(m) {
                Some(why) => format!("📷 {}（{why}）", m.name),
                None => cam_button(&m.name),
            };
            (label, data(&format!("snap:{}", m.id)))
        })
        .collect();
    let rows: Buttons = buttons.chunks(2).map(<[_]>::to_vec).collect();
    p.bot
        .send_text(chat, "其它摄像头，点哪个看哪个：", &quiet(rows))
        .await?;
    Ok(())
}

// ── 天线 ────────────────────────────────────────────────────────────────

async fn antenna_view(p: &Panel) -> Result<(String, Buttons)> {
    let monitors = p.api.monitors().await?;
    let m = monitors
        .iter()
        .find(|m| m.rotator.is_some())
        .context("没有装在天线云台上的摄像头")?;
    let tracking = m.arm.as_ref().is_some_and(|a| a.state == "armed");
    let state = p.api.rotator(m.id).await?;
    let mut text = format!("📡 {}：{}", m.name, rotator_line(&state));
    if m.powered == Some(false) {
        text.push_str("\n摄像头现在没通电（不是故障），看不了画面。");
    }
    let _ = write!(
        text,
        "\nAI 跟踪{}（打开后云台会自己转去跟街上的人和车）",
        if tracking { "开着" } else { "关着" }
    );
    let track = if tracking {
        (
            "🤖 关闭 AI 跟踪".to_string(),
            data(&format!("track:off:{}", m.id)),
        )
    } else {
        (
            "🤖 打开 AI 跟踪".to_string(),
            data(&format!("track:on:{}", m.id)),
        )
    };
    let buttons = vec![vec![
        track,
        ("🏠 回守望位".into(), data(&format!("homepos:{}", m.id))),
        ("📷 看画面".into(), data(&format!("snap:{}", m.id))),
    ]];
    Ok((text, buttons))
}

async fn show_antenna(p: &Panel, chat: &str) -> Result<()> {
    let (text, buttons) = antenna_view(p).await?;
    p.bot.send_text(chat, &text, &quiet(buttons)).await?;
    Ok(())
}

// ── inline buttons ──────────────────────────────────────────────────────

fn answer(toast: &str) -> Option<CallbackAnswer> {
    Some(CallbackAnswer {
        toast: toast.to_string(),
        clear_buttons: false,
    })
}

fn on_press(press: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let rest = press.data.strip_prefix(P)?.to_string();
    let (chat, message) = (press.chat_id?.to_string(), press.message_id?);
    let p = PANEL.get()?;
    if !p.send_to.iter().any(|c| c.trim() == chat) {
        return answer("这个对话不接收监控推送");
    }
    let account = account_of(press.user, &chat);
    let toast = match rest.as_str() {
        "arm:off" => "确定吗？",
        "arm:on" | "arm:offok" => "正在设置…",
        "arm:cancel" => "已取消",
        "push:on" | "push:off" => "已设置",
        r if r.starts_with("snap:") => "正在取画面…",
        r if r.starts_with("clip:") => "开始录 15 秒…",
        r if r.starts_with("track:") => "正在设置…",
        r if r.starts_with("homepos:") => "正在回守望位…",
        _ => return None,
    };
    tokio::spawn(async move {
        if let Err(e) = handle(p, &rest, &chat, message, &account).await {
            let _ = p
                .bot
                .send_text(&chat, &format!("出错了：{e:#}"), &quiet(Vec::new()))
                .await;
        }
    });
    answer(toast)
}

async fn handle(p: &Panel, rest: &str, chat: &str, message: i64, account: &str) -> Result<()> {
    let id_of = |s: &str| -> Result<u32> {
        s.rsplit(':')
            .next()
            .and_then(|v| v.parse().ok())
            .context("按钮数据不对")
    };
    match rest {
        "arm:off" => {
            let confirm = vec![vec![
                ("⏸ 确定撤防".into(), data("arm:offok")),
                ("取消".into(), data("arm:cancel")),
            ]];
            p.bot
                .edit(
                    chat,
                    message,
                    "确定把摄像头全部撤防吗？撤防后它们不报警、不打电话，ntfy 也不推。",
                    false,
                    &confirm,
                )
                .await
        }
        "arm:on" | "arm:offok" => {
            let state = if rest == "arm:on" {
                "armed"
            } else {
                "disarmed"
            };
            p.api
                .set_arm(&json!({"monitor_id": "all", "state": state}), account)
                .await?;
            let v = p.api.status().await?;
            let head = if state == "armed" {
                "✅ 已全部布防"
            } else {
                "⏸ 已全部撤防"
            };
            p.bot
                .edit(
                    chat,
                    message,
                    &format!("{head}\n{}", arm_text(&v)),
                    false,
                    &arm_buttons(),
                )
                .await
        }
        "arm:cancel" => {
            let v = p.api.status().await?;
            p.bot
                .edit(
                    chat,
                    message,
                    &format!("已取消，布防不变。\n{}", arm_text(&v)),
                    false,
                    &arm_buttons(),
                )
                .await
        }
        "push:on" | "push:off" => {
            let on = rest == "push:on";
            let reply = super::set_alerts(&p.workspace, &p.send_to, chat, on)?;
            p.bot
                .edit(
                    chat,
                    message,
                    &format!("{reply}\n\n{}", push_text(p, chat)),
                    false,
                    &push_buttons(),
                )
                .await
        }
        r if r.starts_with("snap:") => snap_by_id(p, chat, id_of(r)?).await,
        r if r.starts_with("clip:") => {
            let id = id_of(r)?;
            let job = p.api.clip_live(id, 15, account).await?;
            Store::open(&p.workspace)?.save_job(&job, chat)?;
            p.bot
                .send_text(
                    chat,
                    "🎬 正在录 15 秒，约 25 秒后发到这里",
                    &SendOpts {
                        silent: true,
                        parent: Some(message),
                        buttons: Vec::new(),
                    },
                )
                .await?;
            Ok(())
        }
        r if r.starts_with("track:") => {
            let id = id_of(r)?;
            let state = if r.starts_with("track:on") {
                "armed"
            } else {
                "disarmed"
            };
            p.api
                .set_arm(&json!({"monitor_id": id, "state": state}), account)
                .await?;
            let (text, buttons) = antenna_view(p).await?;
            p.bot.edit(chat, message, &text, false, &buttons).await
        }
        r if r.starts_with("homepos:") => {
            let id = id_of(r)?;
            p.api
                .ptz(&json!({"monitor_id": id, "action": "home"}), account)
                .await?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            let (text, buttons) = antenna_view(p).await?;
            p.bot.edit(chat, message, &text, false, &buttons).await
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyboard_is_the_owners_layout() {
        let k = keyboard();
        assert_eq!(k.len(), 4);
        assert_eq!(k[0], ["📊 状态", "🛡 布防", "🔔 推送"]);
        assert_eq!(
            k[1],
            ["📷 Garage front", "📷 Front yard fix", "📷 Front yard"]
        );
        assert_eq!(k[2], ["📷 Garage Back", "📷 Back yard", "📷 其它摄像头"]);
        assert_eq!(k[3], ["🏠 我在家", "🚪 我出门", "📡 天线"]);
        for (_, d) in arm_buttons()
            .iter()
            .flatten()
            .chain(cam_buttons(9).iter().flatten())
        {
            assert!(d.len() <= 64);
        }
    }

    #[test]
    fn status_reads_naturally() {
        let v = json!({
            "time": "2026-10-18T15:20:00+11:00",
            "arm": {"summary": "院子 6/6 布防 · 屋顶 1 路撤防"},
            "camera_count": {"online": 8, "total": 8, "offline": [], "unpowered": [{"id": 9, "name": "Yagi ANT"}]},
            "ai": {"online": true, "frame_ms": 196.4, "analyzing": 1},
            "push": {"ntfy": false, "elfclaw": true},
            "nvr": {"configured": true, "online": true, "disk_ok": true, "days": 15, "recording_ok": true},
            "light": {"night": false, "sunrise": "2026-10-18T06:02:00+11:00", "sunset": "2026-10-18T19:16:00+11:00"},
            "faults": [],
            "presence": {"home": true, "until": "2026-10-18T15:50:00+11:00", "account": "鸡妈母"},
            "today": {"alarms": 120, "level3": 1, "level2": 4, "level1": 115, "delivered": 5, "calls": 1,
                      "family": 9, "acquainted": 2, "strangers": 3, "passers": 40, "own_vehicle": 6, "moving_vehicles": 2, "plates": 7},
            "last_alarm": {"time": "2026-10-18T14:18:00+11:00", "monitor_name": "Garage front", "cause_zh": "区域入侵", "description": "认出车：我的车"},
            "call_feedback": {"recent": {"calls": 1, "useful": 1, "useless": 0, "no_feedback": 0}},
            "cameras": [{"name": "Garage Back", "last": {"time": "2026-10-18T15:00:00+11:00", "text": "认出 kasim"}, "today": {"alarms": 12}}]
        });
        let t = status_text(&v);
        for want in [
            "📊 监控状态 15:20",
            "🏠 有人在家（鸡妈母 按的，到 15:50）",
            "🛡 院子 6/6 布防 · 屋顶 1 路撤防",
            "📷 摄像头 8/8 在线；没通电：Yagi ANT",
            "🤖 本地 AI 在线，每帧 196 毫秒，正在细看 1 路",
            "🔔 elfClaw 连着，ntfy 关",
            "💽 NVR 正常，可回看 15 天",
            "🌗 现在是白天，日出 06:02，日落 19:16",
            "📈 今天报警 120 条（打电话级 1、推送级 4、只记录 115），送达 5、打电话 1",
            "🚨 最近一条推送级报警：14:18 Garage front 区域入侵：认出车：我的车",
            "☎️ 今天电话 1 次：有用 1、没用 0、没反馈 0",
            "· Garage Back：15:00 认出 kasim（今天 12 条）",
        ] {
            assert!(t.contains(want), "missing {want:?} in:\n{t}");
        }
        assert!(status_text(
            &json!({"presence": {"home": false, "away_since": "2026-10-18T08:00:00+11:00"}})
        )
        .contains("🚪 没人在家（08:00 按了出门）"));
    }

    #[test]
    fn arm_text_lists_each_camera() {
        let v = json!({"arm": {"summary": "院子 5/6 布防"}, "cameras": [
            {"name": "Garage front", "arm": "armed"},
            {"name": "Back yard", "arm": "disarmed", "arm_until": "2026-10-18T18:00:00+11:00"},
            {"name": "Yagi ANT"}
        ]});
        assert_eq!(
            arm_text(&v),
            "🛡 院子 5/6 布防\n· Garage front：布防\n· Back yard：撤防到 18:00"
        );
    }
}
