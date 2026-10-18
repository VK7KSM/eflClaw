//! Push texts. Plain text (no Telegram parse mode), Chinese, short.

use super::api::Event;
use std::fmt::Write as _;

/// `14:47:06` from `2026-10-14T14:47:06+11:00` (cctv sends local times).
pub fn clock(iso: &str) -> String {
    iso.split('T')
        .nth(1)
        .map(|t| t.chars().take(8).collect())
        .unwrap_or_else(|| iso.to_string())
}

fn icon(e: &Event) -> &'static str {
    match (e.kind.as_str(), e.cause.as_deref()) {
        ("recovery", _) => "✅",
        ("system", Some("Device offline")) => "📵",
        ("system", Some("Disk error")) => "💽",
        ("system", _) => "⚙️",
        _ if e.level >= 3 => "🚨",
        _ => "⚠️",
    }
}

fn target_zh(target: Option<&str>) -> Option<&'static str> {
    match target? {
        "human" => Some("画面里有人"),
        "vehicle" => Some("画面里有车"),
        _ => None,
    }
}

const MAX_DESCRIPTION: usize = 800;

/// The caption of an event push. `false_alarm` adds the AI's verdict.
pub fn caption(e: &Event, false_alarm: bool) -> String {
    let what = e
        .cause_zh
        .clone()
        .or_else(|| e.cause.clone())
        .unwrap_or_else(|| "监控事件".to_string());
    let what = if e.kind == "recovery" {
        format!("已恢复：{what}")
    } else {
        what
    };
    let what = if e.test {
        format!("【测试】{what}")
    } else {
        what
    };
    let mut lines = vec![match e.monitor_name.as_deref() {
        Some(m) => format!("{} {what} · {m}", icon(e)),
        None => format!("{} {what}", icon(e)),
    }];
    if let Some(d) = e.description.as_deref().filter(|d| !d.trim().is_empty()) {
        // A photo caption holds at most 1024 characters; the description
        // grows when cctv appends its second-opinion review.
        let d = d.trim();
        if d.chars().count() > MAX_DESCRIPTION {
            let cut: String = d.chars().take(MAX_DESCRIPTION).collect();
            lines.push(format!("{cut}…"));
        } else {
            lines.push(d.to_string());
        }
    } else if let Some(t) = target_zh(e.target.as_deref()) {
        lines.push(t.to_string());
    }
    if let Some(n) = e.offline_cameras.filter(|n| *n > 0) {
        lines.push(format!("现在共 {n} 台摄像头连不上"));
    }
    if false_alarm {
        lines.push("（AI 判断可能是误报）".to_string());
    }
    if let Some(start) = e.start.as_deref() {
        lines.push(format!("时间 {}", clock(start)));
    }
    if let Some(url) = e.web_url.as_deref() {
        lines.push(url.to_string());
    }
    lines.join("\n")
}

/// Longest report text pushed (a Telegram message holds 4096 characters).
const MAX_REPORT: usize = 3500;

/// The push of a cctv summary (`kind = "report"`).
pub fn report(e: &Event) -> String {
    let span = match (e.from.as_deref(), e.to.as_deref()) {
        (Some(f), Some(t)) => format!("（{}–{}）", clock(f), clock(t)),
        _ => String::new(),
    };
    let title = if e.report_kind.as_deref() == Some("daily") {
        "📋 监控日报".to_string()
    } else {
        format!("📝 监控小结{span}")
    };
    let mut lines = vec![title];
    if let Some(t) = e.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        if t.chars().count() > MAX_REPORT {
            let cut: String = t.chars().take(MAX_REPORT).collect();
            lines.push(format!("{cut}…"));
        } else {
            lines.push(t.to_string());
        }
    }
    if !e.attention.is_empty() {
        lines.push("需要留意：".to_string());
        for a in &e.attention {
            let what = if a.kind == "plate" {
                format!("车牌 {}", a.value)
            } else {
                a.value.clone()
            };
            lines.push(if a.reason.trim().is_empty() {
                format!("• {what}")
            } else {
                format!("• {what}：{}", a.reason.trim())
            });
        }
    }
    lines.join("\n")
}

/// The antenna rotator's state in one line.
pub fn rotator_line(v: &serde_json::Value) -> String {
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

/// One line for the phone/SMS: what and where.
pub fn headline(e: &Event) -> String {
    let what = e
        .cause_zh
        .as_deref()
        .or(e.cause.as_deref())
        .unwrap_or("监控报警");
    match (e.monitor_name.as_deref(), e.offline_cameras) {
        (_, Some(n)) if e.kind == "system" && n >= 2 => format!("{n} 台摄像头连不上"),
        (Some(m), _) => format!("{m} {what}"),
        (None, _) => what.to_string(),
    }
}

/// `15 秒录像`, plus a warning when the NVR had a gap.
pub fn clip_caption(duration: Option<f64>, requested: Option<f64>) -> String {
    let Some(d) = duration else {
        return "🎞 录像".to_string();
    };
    let mut s = format!("🎞 {d:.0} 秒录像");
    if requested.is_some_and(|r| r - d > 2.0) {
        s.push_str("（录像不完整，NVR 那段时间有缺口）");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alarm() -> Event {
        Event {
            event_id: Some(1),
            kind: "alarm".into(),
            level: 3,
            monitor_name: Some("Garage Back".into()),
            cause: Some("Intrusion".into()),
            cause_zh: Some("区域入侵".into()),
            target: Some("human".into()),
            start: Some("2026-10-14T02:13:05+11:00".into()),
            web_url: Some("https://cctv.example/#/events/1".into()),
            ..Event::default()
        }
    }

    #[test]
    fn reports_read_naturally() {
        let r = Event {
            kind: "report".into(),
            level: 2,
            report_id: Some(7),
            report_kind: Some("periodic".into()),
            from: Some("2026-10-15T14:55:00+11:00".into()),
            to: Some("2026-10-15T15:10:00+11:00".into()),
            text: Some("前院有一辆车停了 10 分钟。".into()),
            attention: vec![
                super::super::api::Attention {
                    kind: "plate".into(),
                    value: "ZZZ000".into(),
                    reason: "第二次出现".into(),
                },
                super::super::api::Attention {
                    kind: "note".into(),
                    value: "侧门没关".into(),
                    reason: String::new(),
                },
            ],
            ..Event::default()
        };
        assert_eq!(
            report(&r),
            "📝 监控小结（14:55:00–15:10:00）\n前院有一辆车停了 10 分钟。\n需要留意：\n• 车牌 ZZZ000：第二次出现\n• 侧门没关"
        );
        let daily = Event {
            report_kind: Some("daily".into()),
            attention: vec![],
            ..r
        };
        assert!(report(&daily).starts_with("📋 监控日报\n"));
    }

    #[test]
    fn captions_read_naturally() {
        let c = caption(&alarm(), false);
        assert_eq!(
            c,
            "🚨 区域入侵 · Garage Back\n画面里有人\n时间 02:13:05\nhttps://cctv.example/#/events/1"
        );
        let mut e = alarm();
        e.level = 2;
        e.description = Some("1 个陌生人在门口".into());
        let c = caption(&e, true);
        assert!(
            c.starts_with("⚠️ 区域入侵 · Garage Back\n1 个陌生人在门口\n（AI 判断可能是误报）"),
            "{c}"
        );
        let test = Event {
            test: true,
            ..alarm()
        };
        assert!(caption(&test, false).starts_with("🚨 【测试】区域入侵 · Garage Back"));
        let long = Event {
            description: Some("字".repeat(2000)),
            ..alarm()
        };
        assert!(
            caption(&long, false).chars().count() < 1024,
            "fits a photo caption"
        );

        let off = Event {
            kind: "system".into(),
            level: 3,
            cause: Some("Device offline".into()),
            cause_zh: Some("设备离线".into()),
            monitor_name: Some("Back yard".into()),
            offline_cameras: Some(2),
            ..Event::default()
        };
        assert!(caption(&off, false).contains("📵 设备离线 · Back yard\n现在共 2 台摄像头连不上"));
        assert_eq!(headline(&off), "2 台摄像头连不上");
        assert_eq!(headline(&alarm()), "Garage Back 区域入侵");
        let rec = Event {
            kind: "recovery".into(),
            ..off
        };
        assert!(caption(&rec, false).starts_with("✅ 已恢复：设备离线"));
    }

    #[test]
    fn clip_captions_flag_gaps() {
        assert_eq!(clip_caption(Some(15.0), None), "🎞 15 秒录像");
        assert!(clip_caption(Some(80.0), Some(120.0)).contains("不完整"));
        assert_eq!(clock("2026-10-14T14:47:06+11:00"), "14:47:06");
    }
}
