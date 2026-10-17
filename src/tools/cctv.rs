//! elfClaw 2026-10-14: home CCTV from the chat — look, look back, move the PTZ
//! camera (`cctv`), and arm/disarm (`cctv_arm`, kept separate so it can sit on
//! the approval list). Pictures and clips go straight into the conversation
//! (the caller's Telegram chat), not through the model's reply.

use super::traits::{Tool, ToolResult};
use crate::cctv::api::{CctvApi, Monitor};
use crate::cctv::tg::{Bot, SendOpts};
use crate::providers::{self, ChatMessage, ChatRequest, Provider, ProviderRuntimeOptions};
use crate::security::SecurityPolicy;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, FixedOffset, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Instructions for looking at a camera picture.
const LOOK_PROMPT: &str =
    "你在看用户自己家里监控摄像头的画面。用中文简洁地说清楚画面里看得到的东西：\
有没有人（几个、在哪、在做什么、穿着）、车辆（停着还是在动）、动物、门和大门的状态、\
任何异常。只说看得到的，看不清就说看不清，不要编。用户有具体问题时先回答问题。";

/// The model that looks at pictures (the chat model; Gemini sees images).
struct Vision {
    provider: Box<dyn Provider>,
    model: String,
}

/// Shared by both tools.
pub struct CctvLink {
    api: CctvApi,
    bot: Bot,
    tz: Tz,
    workspace: PathBuf,
    send_to: Vec<String>,
    vision: Option<Vision>,
}

fn vision_from(config: &crate::config::Config) -> Option<Vision> {
    let options = ProviderRuntimeOptions {
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_level: Some(1),
        ..ProviderRuntimeOptions::default()
    };
    match providers::create_resilient_provider_with_options(
        config.default_provider.as_deref().unwrap_or("gemini"),
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &options,
    ) {
        Ok(provider) => Some(Vision {
            provider,
            model: config
                .default_model
                .clone()
                .unwrap_or_else(|| "gemini-3.5-flash".into()),
        }),
        Err(e) => {
            tracing::warn!("CCTV pictures will not be described: {e:#}");
            None
        }
    }
}

impl CctvLink {
    pub fn new(config: &crate::config::Config, bot_token: &str) -> Result<Self> {
        Ok(Self {
            api: CctvApi::new(&config.cctv.base_url)?,
            bot: Bot::new(bot_token),
            tz: config
                .cctv
                .timezone
                .parse()
                .unwrap_or(chrono_tz::Australia::Sydney),
            workspace: config.workspace_dir.clone(),
            send_to: config.cctv.send_to.clone(),
            vision: vision_from(config),
        })
    }

    /// What the model sees in `jpeg` (`what` says which camera and when).
    /// Never fails: a failure becomes the text the agent gets.
    async fn look(&self, jpeg: &[u8], what: &str, question: Option<&str>) -> String {
        let Some(v) = &self.vision else {
            return "（没有能看图的模型，无法分析画面）".into();
        };
        let mut user = format!("这是{what}。");
        if let Some(q) = question.map(str::trim).filter(|q| !q.is_empty()) {
            let _ = write!(user, "\n用户的问题：{q}");
        }
        let _ = write!(
            user,
            "\n[IMAGE:data:image/jpeg;base64,{}]",
            base64::engine::general_purpose::STANDARD.encode(jpeg)
        );
        let messages = [ChatMessage::system(LOOK_PROMPT), ChatMessage::user(user)];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
        };
        match tokio::time::timeout(
            Duration::from_secs(90),
            v.provider.chat(request, &v.model, 0.2),
        )
        .await
        {
            Ok(Ok(r)) => r
                .text
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| "（模型没有给出描述）".into()),
            Ok(Err(e)) => format!("（看图失败：{e:#}）"),
            Err(_) => "（看图超时）".into(),
        }
    }

    async fn camera(&self, wanted: Option<&str>, need_ptz: bool) -> Result<Monitor> {
        let monitors = self.api.monitors().await?;
        let names = || {
            monitors
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join("、")
        };
        let found = match wanted.map(str::trim).filter(|w| !w.is_empty()) {
            Some(w) => find_camera(&monitors, w),
            None if need_ptz => monitors.iter().find(|m| m.ptz),
            None => None,
        };
        let m = found.with_context(|| {
            format!(
                "没有找到摄像头「{}」。现有：{}",
                wanted.unwrap_or(""),
                names()
            )
        })?;
        if need_ptz && !m.ptz {
            bail!(
                "{} 没有云台，能转动的是：{}",
                m.name,
                monitors
                    .iter()
                    .filter(|m| m.ptz)
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>()
                    .join("、")
            );
        }
        Ok(m.clone())
    }

    fn now_iso(&self) -> String {
        Utc::now()
            .with_timezone(&self.tz)
            .format("%Y-%m-%dT%H:%M:%S%:z")
            .to_string()
    }

    async fn send_photo(&self, chat: &str, jpeg: Vec<u8>, caption: &str) -> Result<()> {
        self.bot
            .send_photo(chat, jpeg, caption, &SendOpts::default())
            .await
            .map(|_| ())
    }
}

/// Camera by number, exact name, or name containing the words given.
fn find_camera<'a>(monitors: &'a [Monitor], wanted: &str) -> Option<&'a Monitor> {
    let w = wanted.to_lowercase();
    if let Ok(id) = w.parse::<u32>() {
        return monitors.iter().find(|m| m.id == id);
    }
    monitors
        .iter()
        .find(|m| m.name.to_lowercase() == w)
        .or_else(|| monitors.iter().find(|m| m.name.to_lowercase().contains(&w)))
        .or_else(|| {
            let words: Vec<&str> = w.split_whitespace().collect();
            monitors.iter().find(|m| {
                let n = m.name.to_lowercase();
                !words.is_empty() && words.iter().all(|x| n.contains(x))
            })
        })
}

/// `HH:MM` (today; yesterday if that is still ahead), `YYYY-MM-DD HH:MM`, or
/// RFC 3339 → local RFC 3339.
fn parse_time(s: &str, tz: Tz, now: DateTime<Utc>) -> Result<DateTime<FixedOffset>> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t);
    }
    let local_now = now.with_timezone(&tz);
    let naive = if let Ok(t) =
        NaiveTime::parse_from_str(s, "%H:%M").or_else(|_| NaiveTime::parse_from_str(s, "%H:%M:%S"))
    {
        let today = local_now.date_naive().and_time(t);
        if today > local_now.naive_local() {
            today - chrono::Duration::days(1)
        } else {
            today
        }
    } else {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
            .with_context(|| format!("看不懂的时间：{s}（用 HH:MM 或 YYYY-MM-DD HH:MM）"))?
    };
    let local = tz
        .from_local_datetime(&naive)
        .earliest()
        .with_context(|| format!("不存在的时间：{s}"))?;
    Ok(local.fixed_offset())
}

fn caller_chat() -> Option<String> {
    let c = crate::tools::caller_context::current_caller()?;
    (c.channel == "telegram" && !c.reply_target.is_empty()).then(|| {
        c.reply_target
            .split(':')
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

fn ok(output: impl Into<String>) -> ToolResult {
    ToolResult {
        success: true,
        output: output.into(),
        error: None,
    }
}

fn fail(error: impl Into<String>) -> ToolResult {
    ToolResult {
        success: false,
        output: String::new(),
        error: Some(error.into()),
    }
}

fn guard(security: &SecurityPolicy) -> Option<ToolResult> {
    if !security.can_act() {
        return Some(fail("Action blocked: autonomy is read-only"));
    }
    if !security.record_action() {
        return Some(fail("Action blocked: rate limit exceeded"));
    }
    None
}

pub struct CctvTool {
    link: Arc<CctvLink>,
    security: Arc<SecurityPolicy>,
}

impl CctvTool {
    pub fn new(link: Arc<CctvLink>, security: Arc<SecurityPolicy>) -> Self {
        Self { link, security }
    }

    async fn run(&self, args: &Value) -> Result<String> {
        let link = &self.link;
        let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let n = |k: &str| args.get(k).and_then(Value::as_u64);
        let action = s("action").unwrap_or_default();
        let camera = s("camera");
        let question = s("question");
        match action.as_str() {
            "alerts_off" | "alerts_on" => {
                let chat = caller_chat().context("只能在 Telegram 对话里开关监控推送")?;
                Ok(crate::cctv::set_alerts(
                    &link.workspace,
                    &link.send_to,
                    &chat,
                    action == "alerts_on",
                )?
                .to_string())
            }
            "names" => {
                let (people, vehicles) = (link.api.people().await?, link.api.vehicles().await?);
                let list = |v: &[crate::cctv::api::Name]| {
                    v.iter()
                        .map(|n| match &n.category_zh {
                            Some(c) => format!("{}（{c}）", n.name),
                            None => n.name.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("、")
                };
                Ok(format!(
                    "已登记的人：{}\n已登记的车：{}",
                    list(&people),
                    list(&vehicles)
                ))
            }
            "groups" => {
                let (people, vehicles) = link.api.auto_groups().await?;
                let line = |g: &Value| {
                    let mut l = format!(
                        "组 {}：{}，{} 张",
                        g["id"],
                        g["display"].as_str().unwrap_or(""),
                        g["size"]
                    );
                    if let Some(w) = g["when"].as_str() {
                        let _ = write!(l, "，多在{w}");
                    }
                    if let Some(n) = g["suggest"]["name"].as_str() {
                        let _ = write!(l, "，可能是 {n}（只是建议）");
                    }
                    l
                };
                let list = |v: &Value| {
                    v.as_array()
                        .map(|a| a.iter().take(20).map(line).collect::<Vec<_>>().join("\n"))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "（没有）".into())
                };
                Ok(format!(
                    "还没认领的自动外号（cctv 自动把同一个人或车归成一组，外号不是真名）：\n人：\n{}\n车：\n{}",
                    list(&people),
                    list(&vehicles)
                ))
            }
            "group_label" | "group_ignore" => {
                let id = n("group_id")
                    .or_else(|| s("group_id").and_then(|v| v.trim().parse().ok()))
                    .context("要给组号 group_id")?;
                let id = i64::try_from(id)?;
                let note = if action == "group_ignore" {
                    link.api.group_ignore(id).await?
                } else {
                    let name = s("name").context("要给名字 name")?;
                    link.api
                        .group_label(id, name.trim(), s("category").as_deref())
                        .await?
                };
                Ok(format!("cctv 的说明（请原样转告用户）：{note}"))
            }
            "set_category" => {
                let kind = s("kind").unwrap_or_else(|| "person".into());
                let name = s("name").context("要给名字 name")?;
                let category = s("category").context("要给分类 category")?;
                let allowed: &[&str] = if kind == "vehicle" {
                    &crate::cctv::api::VEHICLE_CATEGORIES
                } else {
                    &crate::cctv::api::PERSON_CATEGORIES
                };
                if !allowed.contains(&category.as_str()) {
                    bail!("{kind} 的分类只能是：{}", allowed.join("、"));
                }
                let note = link.api.set_category(&kind, &name, &category).await?;
                Ok(format!("cctv 的说明（请原样转告用户）：{note}"))
            }
            "objects" | "label" | "unlabel" => {
                let id = n("event_id")
                    .or_else(|| {
                        s("event_id").and_then(|v| v.trim().trim_start_matches('#').parse().ok())
                    })
                    .context("要给报警的事件编号 event_id")?;
                let id = i64::try_from(id)?;
                let object = args.get("object").cloned().unwrap_or(Value::Null);
                match action.as_str() {
                    "objects" => {
                        let objects = link.api.objects(id).await?;
                        if objects.is_empty() {
                            return Ok(
                                "这条报警没有保存人和车（AI 改版前的报警，或者已超过 7 天）。"
                                    .into(),
                            );
                        }
                        let lines: Vec<String> =
                            objects.iter().map(crate::cctv::label::describe).collect();
                        Ok(format!("事件 {id} 的目标：\n{}", lines.join("\n")))
                    }
                    "label" => {
                        let name = s("name")
                            .map(|v| v.trim().to_string())
                            .filter(|v| !v.is_empty())
                            .context("要给名字 name")?;
                        let object = match object {
                            Value::Null => bail!("要给目标序号 object（或 \"all\"）"),
                            Value::String(v) if v != "all" => {
                                json!(v.trim().parse::<u32>().context("object 是序号或 \"all\"")?)
                            }
                            other => other,
                        };
                        let category = s("category");
                        if let Some(c) = category.as_deref() {
                            let ok = crate::cctv::api::PERSON_CATEGORIES.contains(&c)
                                || crate::cctv::api::VEHICLE_CATEGORIES.contains(&c);
                            if !ok {
                                bail!("分类只能是 family/neighbor/friend/stranger（人）或 own/neighbor/friend/stranger（车）");
                            }
                        }
                        Ok(
                            match link
                                .api
                                .label(id, &object, &name, category.as_deref())
                                .await?
                            {
                                crate::cctv::api::LabelOutcome::Done { note, level } => {
                                    let mut out =
                                        format!("标注完成。cctv 的说明（请原样转告用户）：{note}");
                                    if let Some((old, new)) = level.filter(|(o, n)| o != n) {
                                        let _ = write!(
                                            out,
                                            "\n这条报警从 level {old} 改为 level {new}。"
                                        );
                                    }
                                    out
                                }
                                crate::cctv::api::LabelOutcome::Unlearnable(why) => {
                                    format!("学不了，cctv 的原因（请原样转告用户）：{why}")
                                }
                            },
                        )
                    }
                    _ => {
                        let idx = object
                            .as_u64()
                            .or_else(|| object.as_str().and_then(|v| v.trim().parse().ok()))
                            .context("要给目标序号 object")?;
                        link.api.unlabel(id, u32::try_from(idx)?).await?;
                        Ok(format!("已撤销事件 {id} 目标 {idx} 的标注。"))
                    }
                }
            }
            "scene" => {
                let v = link.api.scene().await?;
                let lines: Vec<String> = v["monitors"]
                    .as_array()
                    .map(|a| a.iter().map(scene_line).collect())
                    .unwrap_or_default();
                if lines.is_empty() {
                    return Ok("现在没有画面数据。".into());
                }
                Ok(format!(
                    "现在各路画面（云台机不在内，停留时间单位是秒）：\n{}",
                    lines.join("\n")
                ))
            }
            "reports" => {
                let limit = u32::try_from(n("limit").unwrap_or(10).clamp(1, 20)).unwrap_or(10);
                let v = link.api.reports(limit).await?;
                let mut out = String::new();
                for r in v["reports"].as_array().into_iter().flatten() {
                    let _ = writeln!(
                        out,
                        "[{} {}] {}",
                        r["time"]
                            .as_str()
                            .map(crate::cctv::format::clock)
                            .unwrap_or_default(),
                        if r["report_kind"] == "daily" || r["kind"] == "daily" {
                            "日报"
                        } else {
                            "小结"
                        },
                        r["text"].as_str().unwrap_or("").trim()
                    );
                }
                if out.is_empty() {
                    out.push_str("最近没有总结。\n");
                }
                let watch = &v["watchlist"];
                if watch.as_array().is_some_and(|a| !a.is_empty()) {
                    let _ = write!(out, "关注名单：{watch}");
                }
                Ok(out)
            }
            "greet" => {
                let m = link
                    .camera(Some(camera.as_deref().context("要给摄像头 camera")?), false)
                    .await?;
                let mode = s("mode").unwrap_or_else(|| "hello".into());
                if mode != "hello" && mode != "flash" {
                    bail!("打招呼方式只有 hello（喇叭说话）和 flash（补光灯闪）");
                }
                link.api.greet(m.id, &mode).await?;
                Ok(format!(
                    "{} 已{}。",
                    m.name,
                    if mode == "hello" {
                        "用喇叭打了招呼（叮咚加一句英文问候）"
                    } else {
                        "用补光灯闪了 HI"
                    }
                ))
            }
            "event_snapshot" => {
                let wanted = n("event_id").or_else(|| {
                    s("event_id").and_then(|v| v.trim().trim_start_matches('#').parse().ok())
                });
                let e = match wanted {
                    Some(id) => link.api.event(i64::try_from(id)?).await?,
                    None => {
                        let mut q = vec![
                            "limit=1".to_string(),
                            format!("min_level={}", n("min_level").unwrap_or(2)),
                        ];
                        if let Some(c) = camera.as_deref() {
                            q.push(format!("monitor={}", link.camera(Some(c), false).await?.id));
                        }
                        link.api
                            .events(&q.join("&"))
                            .await?
                            .events
                            .into_iter()
                            .next()
                            .context("没有找到符合条件的报警")?
                    }
                };
                let url = e.snapshot_url.as_deref().context("这个事件没有快照")?;
                let jpeg = link
                    .api
                    .bytes(url, Duration::from_secs(20))
                    .await?
                    .context("快照还没准备好")?;
                let at = e
                    .start
                    .as_deref()
                    .map(crate::cctv::format::clock)
                    .unwrap_or_default();
                let label = format!(
                    "{} {at} 的报警快照（事件 {}，{}）",
                    e.monitor_name.as_deref().unwrap_or("监控"),
                    e.event_id.unwrap_or_default(),
                    e.cause_zh
                        .as_deref()
                        .or(e.cause.as_deref())
                        .unwrap_or("报警")
                );
                let mut out = label.clone();
                if args.get("send").and_then(Value::as_bool) == Some(true) {
                    if let Some(chat) = caller_chat() {
                        link.send_photo(&chat, jpeg.clone(), &label).await?;
                        out.push_str("，已发到对话里");
                    }
                }
                if let Some(d) = e.description.as_deref().filter(|d| !d.trim().is_empty()) {
                    let _ = write!(out, "\ncctv 当时的 AI 判断：{d}");
                }
                let seen = link
                    .look(&jpeg, &format!("家里监控 {label}"), question.as_deref())
                    .await;
                let _ = write!(out, "\n画面内容：{seen}");
                Ok(out)
            }
            "cameras" => {
                let list = link.api.monitors().await?;
                Ok(list
                    .iter()
                    .map(|m| {
                        let state = match (m.online, m.powered) {
                            (true, _) => "",
                            (false, Some(false)) => "（没通电，不是故障）",
                            (false, _) => "（离线）",
                        };
                        let mut line = format!("{}. {}{state}", m.id, m.name);
                        if m.rotator.is_some() {
                            line.push_str("，装在屋顶天线云台上");
                        }
                        if m.ptz {
                            let presets: Vec<String> = m
                                .presets
                                .iter()
                                .map(|p| format!("{}={}", p.id, p.name))
                                .collect();
                            let _ = write!(line, "，有云台，预置位：{}", presets.join("、"));
                        }
                        if m.rotator.is_some() {
                            // On the antenna camera "armed" means AI tracking.
                            let on = m.arm.as_ref().is_some_and(|a| a.state == "armed");
                            line.push_str(if on {
                                "，AI 跟踪开"
                            } else {
                                "，AI 跟踪关"
                            });
                        } else if let Some(a) = &m.arm {
                            if a.state != "armed" {
                                let _ = write!(
                                    line,
                                    "，已撤防{}",
                                    a.until
                                        .as_deref()
                                        .map(|u| format!("到 {}", crate::cctv::format::clock(u)))
                                        .unwrap_or_default()
                                );
                            }
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "snapshot" => {
                let chat = caller_chat().context("实时画面只能发到 Telegram 对话里")?;
                let m = link.camera(camera.as_deref(), false).await?;
                let jpeg = link.api.live_snapshot(m.id).await?;
                let at = crate::cctv::format::clock(&link.now_iso());
                let what = format!("摄像头 {} 在 {at} 的实时画面", m.name);
                let caption = format!("{} 现在的画面 {at}", m.name);
                let (sent, seen) = tokio::join!(
                    link.send_photo(&chat, jpeg.clone(), &caption),
                    link.look(&jpeg, &what, question.as_deref())
                );
                sent?;
                Ok(format!(
                    "{} 的实时画面（{at}）已发到对话里，用户能看到。\n画面内容：{seen}",
                    m.name
                ))
            }
            "events" => {
                let now = Utc::now();
                let mut q = vec![format!("limit={}", n("limit").unwrap_or(30).min(200))];
                if let Some(c) = camera.as_deref() {
                    q.push(format!("monitor={}", link.camera(Some(c), false).await?.id));
                }
                for k in ["from", "to"] {
                    if let Some(t) = s(k) {
                        let t = parse_time(&t, link.tz, now)?
                            .to_rfc3339()
                            .replace('+', "%2B");
                        q.push(format!("{k}={t}"));
                    }
                }
                q.push(format!("min_level={}", n("min_level").unwrap_or(1)));
                let page = link.api.events(&q.join("&")).await?;
                if page.events.is_empty() {
                    return Ok("这段时间没有记录到事件。".into());
                }
                let lines: Vec<String> = page
                    .events
                    .iter()
                    .map(|e| {
                        let detail = e
                            .description
                            .clone()
                            .or_else(|| {
                                e.target.as_deref().map(|t| {
                                    if t == "human" {
                                        "有人".into()
                                    } else if t == "vehicle" {
                                        "有车".into()
                                    } else {
                                        t.into()
                                    }
                                })
                            })
                            .unwrap_or_default();
                        format!(
                            "#{} {} {} {} {} 级别{}",
                            e.event_id.unwrap_or_default(),
                            e.start
                                .as_deref()
                                .map(crate::cctv::format::clock)
                                .unwrap_or_default(),
                            e.monitor_name.as_deref().unwrap_or(""),
                            e.cause_zh.as_deref().unwrap_or(""),
                            detail,
                            e.level
                        )
                    })
                    .collect();
                Ok(format!(
                    "共 {} 条（最新在前）：\n{}",
                    lines.len(),
                    lines.join("\n")
                ))
            }
            "clip_live" | "clip_history" => {
                let chat = caller_chat().context("视频只能发到 Telegram 对话里")?;
                let m = link.camera(camera.as_deref(), false).await?;
                let (job, wait) = if action == "clip_live" {
                    let secs = u32::try_from(n("seconds").unwrap_or(15).clamp(5, 20)).unwrap_or(15);
                    (
                        link.api.clip_live(m.id, secs).await?,
                        format!("约 {} 秒后", secs + 10),
                    )
                } else {
                    let now = Utc::now();
                    let from = parse_time(
                        &s("from").context("历史录像要给开始时间 from")?,
                        link.tz,
                        now,
                    )?;
                    let to =
                        parse_time(&s("to").context("历史录像要给结束时间 to")?, link.tz, now)?;
                    let secs = (to - from).num_seconds();
                    if secs <= 0 || secs > 300 {
                        bail!("历史录像一次最长 5 分钟，开始要早于结束");
                    }
                    if Utc::now() - to.with_timezone(&Utc) < chrono::Duration::minutes(2) {
                        bail!("录像机只能取 2 分钟以前的录像；更近的画面请用 clip_live 实时录");
                    }
                    let job = link
                        .api
                        .clip_history(m.id, &from.to_rfc3339(), &to.to_rfc3339())
                        .await?;
                    (job, format!("约 {} 分钟后", secs / 60 + 2))
                };
                crate::cctv::store::Store::open(&link.workspace)?.save_job(&job, &chat)?;
                Ok(format!("{} 的录像已在生成，{wait}自动发到对话里。", m.name))
            }
            "ptz" => {
                let chat = caller_chat();
                let m = link.camera(camera.as_deref(), true).await?;
                let ptz_action = s("ptz_action").unwrap_or_else(|| "preset".into());
                let antenna = m.rotator.is_some();
                if ptz_action == "status" {
                    let v = link.api.rotator(m.id).await?;
                    return Ok(format!("{}：{}", m.name, rotator_line(&v)));
                }
                let mut body = json!({"monitor_id": m.id, "action": ptz_action});
                let mut wait_ms = 1500u64;
                let done = match ptz_action.as_str() {
                    "preset" => {
                        let p = s("preset")
                            .or_else(|| n("preset").map(|v| v.to_string()))
                            .context("要给预置位编号或名称 preset")?;
                        let preset = m
                            .presets
                            .iter()
                            .find(|x| x.id.to_string() == p || x.name == p || x.name.contains(&p))
                            .with_context(|| {
                                format!(
                                    "没有预置位「{p}」，有：{}",
                                    m.presets
                                        .iter()
                                        .map(|x| format!("{}={}", x.id, x.name))
                                        .collect::<Vec<_>>()
                                        .join("、")
                                )
                            })?;
                        body["preset"] = preset.id.into();
                        wait_ms = 4000;
                        format!("转到预置位 {}（{}）", preset.id, preset.name)
                    }
                    "move" | "zoom" => {
                        let dir = s("direction").context("要给方向 direction")?;
                        let ms = n("duration_ms").unwrap_or(1000).clamp(100, 5000);
                        body["direction"] = dir.clone().into();
                        body["duration_ms"] = ms.into();
                        if let Some(sp) = n("speed") {
                            body["speed"] = sp.clamp(1, 100).into();
                        }
                        wait_ms = ms + 1500;
                        format!(
                            "{} {dir} {ms} 毫秒",
                            if ptz_action == "move" {
                                "转动"
                            } else {
                                "变焦"
                            }
                        )
                    }
                    "stop" => "停止".to_string(),
                    "goto" if antenna => {
                        let deg = |k: &str| args.get(k).and_then(Value::as_f64);
                        let (az, el) = (
                            deg("az").context("要给方位 az（度，0=正北，顺时针）")?,
                            deg("el").context("要给俯仰 el（度，0=水平）")?,
                        );
                        body["az"] = az.into();
                        body["el"] = el.into();
                        format!("转到方位 {az:.0}°、俯仰 {el:.0}°")
                    }
                    "home" if antenna => "回到守望位".to_string(),
                    "look" if antenna => {
                        let from = link
                            .camera(
                                Some(
                                    s("from_camera")
                                        .context("要给对准哪一路摄像头 from_camera")?
                                        .as_str(),
                                ),
                                false,
                            )
                            .await?;
                        body["from_monitor"] = from.id.into();
                        format!("转向 {} 看的方向", from.name)
                    }
                    "goto" | "home" | "look" => {
                        bail!("{} 不是天线云台，只能 preset/move/zoom/stop", m.name)
                    }
                    other => bail!("不认识的云台动作：{other}"),
                };
                link.api.ptz(&body).await?;
                if antenna {
                    // The rotator turns slowly (about 7°/s); look once it stops.
                    wait_ms = 0;
                    for _ in 0..30 {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        match link.api.rotator(m.id).await {
                            Ok(v) if v["moving"].as_bool() == Some(true) => {}
                            _ => break,
                        }
                    }
                }
                if let Some(chat) = chat {
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    if let Ok(jpeg) = link.api.live_snapshot(m.id).await {
                        let what = format!("云台摄像头 {} {done}后的画面", m.name);
                        let caption = format!("{}：{done}后的画面", m.name);
                        let (_, seen) = tokio::join!(
                            link.send_photo(&chat, jpeg.clone(), &caption),
                            link.look(&jpeg, &what, question.as_deref())
                        );
                        return Ok(format!(
                            "{} 已{done}，新画面已发到对话里。\n画面内容：{seen}",
                            m.name
                        ));
                    }
                }
                Ok(format!("{} 已{done}。", m.name))
            }
            other => bail!("不认识的动作：{other}"),
        }
    }
}

#[async_trait]
impl Tool for CctvTool {
    fn name(&self) -> &str {
        "cctv"
    }

    fn description(&self) -> &str {
        "Home CCTV (the user's own cameras). Actions: cameras (list cameras, PTZ presets, arm state); \
         snapshot (send a live picture of one camera into this chat AND get a description of what is \
         in it); event_snapshot (look at the snapshot of a past alarm: event_id, or the latest alarm \
         of a camera; a pushed alarm's link ends in /events/<event_id>; send=true also posts the \
         picture, only needed when the user has not seen it); alerts_off / alerts_on (stop or \
         resume alarm pushes to this chat; the cameras stay armed; same as the /cctv_off and \
         /cctv_on menu commands); scene (who and what is at each camera right now: people, \
         whether in the yard, how long they stayed, parked cars — for 'is anyone in the yard now', \
         'how long has that car been there'); reports (cctv's recent 15-minute summaries, nightly \
         report and watch list — for 'what happened today'); greet (say hello at a camera: mode hello = chime + spoken \
         English greeting on its speaker, flash = blink HI with its light; camera required); \
         objects / label / unlabel (people and vehicles of an alarm, and teaching cctv's AI who \
         they are: event_id + object index or 'all' + name, e.g. a person's name or '我的车'; a \
         target picture's caption reads '事件 <event_id> · 目标 <object>', so when the user replies \
         to one with a name, call label with those numbers; relay cctv's note or reason verbatim; \
         a person without a clear face is remembered by build and clothes for that day only; \
         '认出是「X」' is a firm recognition, say 'recognised X', never 'maybe'; every name has a \
         category — people 家人 family / 邻居 neighbor / 朋友 friend / 陌生人 stranger, vehicles \
         自家车 own / 邻居车 neighbor / 朋友车 friend / 陌生车 stranger; only family and own stop \
         the alarms; a new name in label takes 'category' — ask the user which if they did not \
         say, without it cctv treats it as friend); names (registered people and vehicles with \
         their categories); set_category (kind person/vehicle, name, category: change a \
         registered name's category; family or own means that person or car never raises an \
         alarm again, so make sure the user really asked for it first; relay cctv's note); groups \
         (cctv's automatic groups: pictures it thinks show the same person or vehicle, under a \
         nickname shown as '〔自动〕…' — a nickname, never a real name; a target in an alarm may say \
         '像是〔自动〕X（组 N）'); group_label (group_id + name [+ category for a new name]: the user \
         says that nickname is really someone — same category rules as label); group_ignore \
         (group_id: ignore that group from now on); events (what happened: \
         optional camera, from/to as HH:MM or YYYY-MM-DD HH:MM, min_level 1-3); clip_live (record \
         5-20 s of live video now and send it); clip_history (recorded video from the NVR, from/to, \
         at most 5 minutes, ending at least 2 minutes ago); ptz (move the PTZ camera: ptz_action \
         preset with preset number or name, move/zoom with direction and duration_ms, or stop; \
         the roof antenna camera 'Yagi ANT' sits on the satellite-antenna rotator and also takes \
         goto (az = compass bearing 0 north clockwise, el = elevation 0 horizontal), home, look \
         (from_camera: point where that camera looks) and status; it is not always powered — \
         'not powered' is normal, not a fault; while the user tracks a satellite with DTrac every \
         command is refused with '正在追星，云台由 DTrac 控制': tell the user that, it is not an error). \
         Camera names are English (e.g. 'Front yard', 'Garage front'); map the user's words to \
         them, call cameras first if unsure. Pictures and videos are delivered by the tool itself. \
         snapshot, event_snapshot and ptz return what the picture shows (pass the user's question \
         as 'question'): answer from that description and never say you cannot see the picture. \
         Event lines start with #event_id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["cameras", "snapshot", "event_snapshot", "events", "clip_live", "clip_history", "ptz", "greet", "scene", "reports", "objects", "label", "unlabel", "names", "set_category", "groups", "group_label", "group_ignore", "alerts_off", "alerts_on"]},
                "category": {"type": "string", "enum": ["family", "own", "neighbor", "friend", "stranger"], "description": "label (new name) / set_category: person family|neighbor|friend|stranger, vehicle own|neighbor|friend|stranger"},
                "kind": {"type": "string", "enum": ["person", "vehicle"], "description": "set_category: a person's or a vehicle's name"},
                "group_id": {"type": "integer", "description": "group_label / group_ignore: the automatic group's number"},
                "object": {"description": "label/unlabel: target index (or \"all\" for label)"},
                "name": {"type": "string", "description": "label: person or vehicle name"},
                "mode": {"type": "string", "enum": ["hello", "flash"], "description": "greet: speaker or light"},
                "question": {"type": "string", "description": "What the user wants to know about the picture"},
                "event_id": {"type": "integer", "description": "Alarm event id (event_snapshot, objects, label, unlabel)"},
                "send": {"type": "boolean", "description": "event_snapshot: also post the picture"},
                "camera": {"type": "string", "description": "Camera name (or number). For ptz it defaults to the PTZ camera."},
                "from": {"type": "string", "description": "Start time: HH:MM (today) or YYYY-MM-DD HH:MM"},
                "to": {"type": "string", "description": "End time, same format"},
                "min_level": {"type": "integer", "minimum": 1, "maximum": 3},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                "seconds": {"type": "integer", "minimum": 5, "maximum": 20},
                "ptz_action": {"type": "string", "enum": ["preset", "move", "zoom", "stop", "goto", "home", "look", "status"]},
                "az": {"type": "number", "description": "ptz goto (antenna): bearing in degrees, 0 = north, clockwise"},
                "el": {"type": "number", "description": "ptz goto (antenna): elevation in degrees, 0 = horizontal"},
                "from_camera": {"type": "string", "description": "ptz look (antenna): point where this camera looks"},
                "preset": {"type": "string", "description": "Preset number or name"},
                "direction": {"type": "string", "enum": ["up", "down", "left", "right", "up_left", "up_right", "down_left", "down_right", "in", "out"]},
                "duration_ms": {"type": "integer", "minimum": 100, "maximum": 5000},
                "speed": {"type": "integer", "minimum": 1, "maximum": 100}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        if let Some(blocked) = guard(&self.security) {
            return Ok(blocked);
        }
        Ok(match self.run(&args).await {
            Ok(out) => ok(out),
            Err(e) => fail(format!("{e:#}")),
        })
    }
}

/// Arm / disarm — on the approval list, so the owner confirms first.
pub struct CctvArmTool {
    link: Arc<CctvLink>,
    security: Arc<SecurityPolicy>,
}

impl CctvArmTool {
    pub fn new(link: Arc<CctvLink>, security: Arc<SecurityPolicy>) -> Self {
        Self { link, security }
    }
}

#[async_trait]
impl Tool for CctvArmTool {
    fn name(&self) -> &str {
        "cctv_arm"
    }

    fn description(&self) -> &str {
        "Arm or disarm home CCTV alarm detection for one camera or all ('all'). Disarmed cameras \
         still record but raise no alarms anywhere: no Telegram, no ntfy, no phone call even for a \
         real intrusion. If the user only does not want Telegram alarm messages, do NOT disarm: use \
         the cctv tool's alerts_off (or the /cctv_off menu). Optional 'until' (HH:MM or YYYY-MM-DD HH:MM) re-arms \
         automatically. Use state 'status' to just read the current state. Exception: on the roof \
         antenna camera 'Yagi ANT' armed/disarmed switches the rotator's AI tracking on/off (it then \
         turns by itself to follow people and cars on the street); it is not touched by 'all', must be \
         named, and must be called 'AI 跟踪' to the user, never '布防'."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "camera": {"type": "string", "description": "Camera name or 'all'"},
                "state": {"type": "string", "enum": ["armed", "disarmed", "status"]},
                "until": {"type": "string", "description": "When to re-arm (only for disarmed)"}
            },
            "required": ["state"]
        })
    }

    async fn execute(&self, args: Value) -> Result<ToolResult> {
        if let Some(blocked) = guard(&self.security) {
            return Ok(blocked);
        }
        let link = &self.link;
        let state = args
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("status");
        let result: Result<String> = async {
            if state == "status" {
                return Ok(link.api.arm_status().await?.to_string());
            }
            let camera = args.get("camera").and_then(Value::as_str).unwrap_or("all");
            let mut antenna = None;
            let monitor: Value = if camera.eq_ignore_ascii_case("all") || camera == "全部" {
                "all".into()
            } else {
                let m = link.camera(Some(camera), false).await?;
                if m.rotator.is_some() {
                    antenna = Some(m.name.clone());
                }
                m.id.into()
            };
            let mut body = json!({"monitor_id": monitor, "state": state});
            if let Some(u) = args
                .get("until")
                .and_then(Value::as_str)
                .filter(|_| state == "disarmed")
            {
                body["until"] = parse_time_future(u, link.tz, Utc::now())?
                    .to_rfc3339()
                    .into();
            }
            link.api.set_arm(&body).await?;
            if let Some(name) = antenna {
                let on = state == "armed";
                return Ok(format!(
                    "已{} {name} 天线云台的 AI 跟踪{}",
                    if on { "打开" } else { "关闭" },
                    if on {
                        "（云台会自己转去跟街上的人和车）"
                    } else {
                        ""
                    }
                ));
            }
            Ok(format!("已设置：{camera} → {state}"))
        }
        .await;
        Ok(match result {
            Ok(out) => ok(out),
            Err(e) => fail(format!("{e:#}")),
        })
    }
}

/// The antenna rotator's state in one line.
fn rotator_line(v: &Value) -> String {
    let mut line = format!(
        "方位 {:.0}°、俯仰 {:.0}°{}",
        v["az"].as_f64().unwrap_or_default(),
        v["el"].as_f64().unwrap_or_default(),
        if v["moving"].as_bool() == Some(true) {
            "，正在转动"
        } else {
            ""
        }
    );
    let by = match v["controller"].as_str().unwrap_or("") {
        "dtrac" => "DTrac 追星",
        "manual" => "手动控制",
        "ai" => "AI 跟踪",
        "look" => "对准某一路摄像头",
        _ => "",
    };
    if !by.is_empty() {
        let _ = write!(line, "，当前由{by}控制");
    }
    if v["at_home"].as_bool() == Some(true) {
        line.push_str("，在守望位");
    }
    if v["ai_track"].as_bool() == Some(true) {
        line.push_str("，AI 跟踪开");
    }
    if let Some(msg) = v["message"].as_str().filter(|m| !m.is_empty()) {
        let _ = write!(line, "（{msg}）");
    }
    line
}

/// One camera of `/scene`: who and what is there now.
fn scene_line(m: &Value) -> String {
    let name = m["name"].as_str().unwrap_or("?");
    if m["stale"].as_bool() == Some(true) {
        return format!("{name}：超过 30 秒没有更新，不确定");
    }
    let empty = Vec::new();
    let persons = m["persons"].as_array().unwrap_or(&empty);
    let vehicles = m["vehicles"].as_array().unwrap_or(&empty);
    let mut parts = Vec::new();
    if !persons.is_empty() {
        let in_yard = persons.iter().filter(|p| p["in_yard"] == true).count();
        let longest = persons
            .iter()
            .filter_map(|p| p["dwell_s"].as_f64())
            .fold(0.0_f64, f64::max);
        parts.push(format!(
            "{} 人（院子里 {in_yard}，停留最长 {longest:.0}）",
            persons.len()
        ));
    }
    if !vehicles.is_empty() {
        let parked = vehicles.iter().filter(|v| v["parked"] == true).count();
        let longest = vehicles
            .iter()
            .filter_map(|v| v["dwell_s"].as_f64())
            .fold(0.0_f64, f64::max);
        parts.push(format!(
            "{} 辆车（停着 {parked}，停留最长 {longest:.0}）",
            vehicles.len()
        ));
    }
    if let Some(a) = m["animals"].as_u64().filter(|a| *a > 0) {
        parts.push(format!("{a} 只动物"));
    }
    if parts.is_empty() {
        format!("{name}：没有人和车")
    } else {
        format!("{name}：{}", parts.join("，"))
    }
}

/// Like `parse_time`, but `HH:MM` means the next such time (re-arm moments
/// are in the future).
fn parse_time_future(s: &str, tz: Tz, now: DateTime<Utc>) -> Result<DateTime<FixedOffset>> {
    let t = parse_time(s, tz, now)?;
    if t.with_timezone(&Utc) <= now && NaiveTime::parse_from_str(s.trim(), "%H:%M").is_ok() {
        return Ok(t + chrono::Duration::days(1));
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitors() -> Vec<Monitor> {
        [
            "Ceiling",
            "Garage Back",
            "Front yard PTZ",
            "Front yard fix",
            "Garage front",
            "Back yard",
            "Front yard",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| Monitor {
            id: u32::try_from(i + 1).unwrap(),
            name: (*n).to_string(),
            online: true,
            ptz: *n == "Front yard PTZ",
            presets: vec![],
            arm: None,
            powered: None,
            rotator: None,
        })
        .collect()
    }

    #[test]
    fn rotator_state_reads_naturally() {
        let v = json!({"az": 181.6, "el": 32.2, "moving": false, "controller": "dtrac",
            "locked": true, "at_home": false, "message": "正在追星，云台由 DTrac 控制"});
        assert_eq!(
            rotator_line(&v),
            "方位 182°、俯仰 32°，当前由DTrac 追星控制（正在追星，云台由 DTrac 控制）"
        );
        let idle = json!({"az": 90, "el": 0, "moving": true, "controller": "idle", "at_home": true, "ai_track": true});
        assert_eq!(
            rotator_line(&idle),
            "方位 90°、俯仰 0°，正在转动，在守望位，AI 跟踪开"
        );
    }

    #[test]
    fn scene_lines_read_naturally() {
        let m = json!({"id": 2, "name": "Garage Back", "state": "yard",
            "persons": [{"track": 3, "in_yard": true, "dwell_s": 40, "seen_s": 45}],
            "vehicles": [{"track": 4, "parked": true, "dwell_s": 600}], "animals": 0, "age_s": 1, "stale": false});
        assert_eq!(
            scene_line(&m),
            "Garage Back：1 人（院子里 1，停留最长 40），1 辆车（停着 1，停留最长 600）"
        );
        assert_eq!(
            scene_line(
                &json!({"name": "Back yard", "persons": [], "vehicles": [], "stale": false})
            ),
            "Back yard：没有人和车"
        );
        assert!(scene_line(&json!({"name": "X", "stale": true})).contains("没有更新"));
    }

    #[test]
    fn cameras_are_found_by_number_name_or_words() {
        let m = monitors();
        assert_eq!(find_camera(&m, "5").unwrap().name, "Garage front");
        assert_eq!(
            find_camera(&m, "front yard").unwrap().name,
            "Front yard",
            "exact name first"
        );
        assert_eq!(find_camera(&m, "PTZ").unwrap().name, "Front yard PTZ");
        assert_eq!(find_camera(&m, "garage back").unwrap().id, 2);
        assert!(find_camera(&m, "roof").is_none());
    }

    #[test]
    fn times_are_read_in_local_time() {
        let tz = chrono_tz::Australia::Sydney;
        let now = DateTime::parse_from_rfc3339("2026-10-14T15:00:00+11:00")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            parse_time("14:00", tz, now).unwrap().to_rfc3339(),
            "2026-10-14T14:00:00+11:00"
        );
        assert_eq!(
            parse_time("16:00", tz, now).unwrap().to_rfc3339(),
            "2026-10-13T16:00:00+11:00",
            "a future HH:MM means yesterday"
        );
        assert_eq!(
            parse_time("2026-10-12 08:30", tz, now)
                .unwrap()
                .to_rfc3339(),
            "2026-10-12T08:30:00+11:00"
        );
        assert!(parse_time("明天", tz, now).is_err());
        assert_eq!(
            parse_time_future("07:00", tz, now).unwrap().to_rfc3339(),
            "2026-10-15T07:00:00+11:00"
        );
    }
}
