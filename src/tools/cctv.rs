//! elfClaw 2026-10-14: home CCTV from the chat — look, look back, move the PTZ
//! camera (`cctv`), and arm/disarm (`cctv_arm`, kept separate so it can sit on
//! the approval list). Pictures and clips go straight into the conversation
//! (the caller's Telegram chat), not through the model's reply.

use super::traits::{Tool, ToolResult};
use crate::cctv::api::{CctvApi, Monitor};
use crate::cctv::tg::{Bot, SendOpts};
use crate::security::SecurityPolicy;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Shared by both tools.
pub struct CctvLink {
    api: CctvApi,
    bot: Bot,
    tz: Tz,
    workspace: PathBuf,
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
        })
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
        match action.as_str() {
            "cameras" => {
                let list = link.api.monitors().await?;
                Ok(list
                    .iter()
                    .map(|m| {
                        let mut line = format!(
                            "{}. {}{}",
                            m.id,
                            m.name,
                            if m.online { "" } else { "（离线）" }
                        );
                        if m.ptz {
                            let presets: Vec<String> = m
                                .presets
                                .iter()
                                .map(|p| format!("{}={}", p.id, p.name))
                                .collect();
                            let _ = write!(line, "，有云台，预置位：{}", presets.join("、"));
                        }
                        if let Some(a) = &m.arm {
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
                link.send_photo(&chat, jpeg, &format!("{} 现在的画面 {at}", m.name))
                    .await?;
                Ok(format!("{} 的实时画面已经发到对话里。", m.name))
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
                            "{} {} {} {} 级别{}",
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
                    other => bail!("不认识的云台动作：{other}"),
                };
                link.api.ptz(&body).await?;
                if let Some(chat) = chat {
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                    if let Ok(jpeg) = link.api.live_snapshot(m.id).await {
                        let _ = link
                            .send_photo(&chat, jpeg, &format!("{}：{done}后的画面", m.name))
                            .await;
                        return Ok(format!("{} 已{done}，新画面已发到对话里。", m.name));
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
         snapshot (send a live picture of one camera into this chat); events (what happened: \
         optional camera, from/to as HH:MM or YYYY-MM-DD HH:MM, min_level 1-3); clip_live (record \
         5-20 s of live video now and send it); clip_history (recorded video from the NVR, from/to, \
         at most 5 minutes, ending at least 2 minutes ago); ptz (move the PTZ camera: ptz_action \
         preset with preset number or name, move/zoom with direction and duration_ms, or stop). \
         Camera names are English (e.g. 'Front yard', 'Garage front'); map the user's words to \
         them, call cameras first if unsure. Pictures and videos are delivered by the tool itself; \
         just tell the user they were sent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["cameras", "snapshot", "events", "clip_live", "clip_history", "ptz"]},
                "camera": {"type": "string", "description": "Camera name (or number). For ptz it defaults to the PTZ camera."},
                "from": {"type": "string", "description": "Start time: HH:MM (today) or YYYY-MM-DD HH:MM"},
                "to": {"type": "string", "description": "End time, same format"},
                "min_level": {"type": "integer", "minimum": 1, "maximum": 3},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200},
                "seconds": {"type": "integer", "minimum": 5, "maximum": 20},
                "ptz_action": {"type": "string", "enum": ["preset", "move", "zoom", "stop"]},
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
        "Arm or disarm home CCTV alarm pushes for one camera or all ('all'). Disarmed cameras still \
         record but their alarms are not pushed. Optional 'until' (HH:MM or YYYY-MM-DD HH:MM) re-arms \
         automatically. Use state 'status' to just read the current state."
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
            let monitor: Value = if camera.eq_ignore_ascii_case("all") || camera == "全部" {
                "all".into()
            } else {
                link.camera(Some(camera), false).await?.id.into()
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
            Ok(format!("已设置：{camera} → {state}"))
        }
        .await;
        Ok(match result {
            Ok(out) => ok(out),
            Err(e) => fail(format!("{e:#}")),
        })
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
        })
        .collect()
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
