//! Push texts. Plain text (no Telegram parse mode), Chinese, short.

use super::api::Event;

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
