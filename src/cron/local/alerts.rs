//! Emergency alerts, polled every five minutes. Rules only, except for the
//! breaking-news judgement.
//!
//! - BOM official warnings for the home location — severe thunderstorm and
//!   severe weather warnings are P1 (sent even in the quiet hours);
//! - BOM's hourly gust forecast (and Open-Meteo for hail) for the next 24
//!   hours against the antenna thresholds — P2, so a warning comes hours
//!   before the weather;
//! - LiveTraffic: a crash marked major near home, or a crash on a school-run
//!   road during the school run — P2;
//! - RFS: a fire nearby at Watch and Act (P2) or Emergency Warning (P1);
//! - breaking news of attacks and violence, judged by the model only after a
//!   code-side keyword filter (see `breaking`).
//!
//! Each alert has a key and a level; it is sent once, and again only if its
//! level rises (a Watch and Act fire becoming an Emergency Warning). A P2 alert
//! that falls in the quiet hours is not marked as sent, so the first poll
//! after the quiet window picks it up if it still holds.

use super::feeds::{self, Fire, HourForecast, Incident, MeteoHour, Warning};
use super::{open_state, LocalConfig, NO_REPLY};
use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Timelike, Utc};
use std::fmt::Write as _;

/// How far ahead the forecast is scanned.
const FORECAST_HOURS: i64 = 24;
/// Lines per message; anything beyond is summarised as a count.
const MAX_LINES: usize = 10;
/// School-run window for route incidents, local time.
const SCHOOL_RUN_START: u32 = 7 * 60;
const SCHOOL_RUN_END: u32 = 9 * 60;

/// (source, minutes between fetches). The job itself runs every five minutes.
const CADENCE: &[(&str, i64)] = &[
    ("bom_warnings", 5),
    ("forecast", 30),
    ("traffic", 5),
    ("rfs", 15),
    ("news", 5),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    /// Urgent: sent at any hour.
    P1,
    /// Sent outside the quiet hours.
    P2,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Alert {
    pub priority: Priority,
    /// Identity for de-duplication.
    pub key: String,
    /// Sent again only when this rises.
    pub level: i64,
    pub text: String,
}

// ── rules ────────────────────────────────────────────────────────────────

fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    match (day - today).num_days() {
        0 => "今天".into(),
        1 => "明天".into(),
        2 => "后天".into(),
        _ => day.format("%m-%d").to_string(),
    }
}

/// Forecast gusts, storms and hail in the next `FORECAST_HOURS` hours.
///
/// Gusts come from BOM (the official forecast); Open-Meteo stands in only when
/// BOM returned nothing. Hail comes from Open-Meteo, whose WMO codes separate
/// it from an ordinary storm, which BOM's hourly icons do not.
pub fn forecast_alerts(
    bom: &[HourForecast],
    meteo: &[MeteoHour],
    now: DateTime<Utc>,
    local: &LocalConfig,
) -> Vec<Alert> {
    let tz = local.tz();
    let end = now + Duration::hours(FORECAST_HOURS);
    let today = now.with_timezone(&tz).date_naive();
    let meteo_at = |h: &MeteoHour| {
        tz.from_local_datetime(&h.time)
            .earliest()
            .map(|t| t.with_timezone(&Utc))
    };
    let meteo_ahead: Vec<(DateTime<Utc>, &MeteoHour)> = meteo
        .iter()
        .filter_map(|h| Some((meteo_at(h)?, h)))
        .filter(|(t, _)| *t >= now && *t <= end)
        .collect();

    // (time, gust) of the strongest gust ahead.
    let bom_gusts = bom
        .iter()
        .filter(|h| h.time >= now && h.time <= end)
        .filter_map(|h| Some((h.time, h.gust_kmh?)));
    let peak = bom_gusts.max_by(|a, b| a.1.total_cmp(&b.1)).or_else(|| {
        meteo_ahead
            .iter()
            .filter_map(|(t, h)| Some((*t, h.gust_kmh?)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
    });

    let when = |t: DateTime<Utc>| {
        let l = t.with_timezone(&tz);
        let hours = (t - now).num_minutes().max(0) / 60;
        let ahead = if hours == 0 {
            "一小时内".to_string()
        } else {
            format!("约 {hours} 小时后")
        };
        (
            l.date_naive(),
            format!(
                "{} {} 点前后（{ahead}）",
                day_label(l.date_naive(), today),
                l.hour()
            ),
        )
    };

    let mut out = Vec::new();
    if let Some((t, gust)) = peak {
        let a = local.antenna;
        let level = if gust >= a.lower_gust_kmh {
            2
        } else if gust >= a.check_gust_kmh {
            1
        } else {
            0
        };
        if level > 0 {
            let (day, label) = when(t);
            let action = if level == 2 {
                "建议放倒或拆下天线"
            } else {
                "请检查天线风绳是否固定好"
            };
            out.push(Alert {
                priority: Priority::P2,
                key: format!("fc-gust:{day}"),
                level,
                text: format!("📡 预报{label}阵风 {gust:.0} km/h——{action}"),
            });
        }
    }

    let storm = bom
        .iter()
        .filter(|h| h.time >= now && h.time <= end && h.icon == "storm")
        .map(|h| h.time)
        .chain(
            meteo_ahead
                .iter()
                .filter(|(_, h)| h.storm())
                .map(|(t, _)| *t),
        )
        .min();
    if let Some(t) = storm {
        let (day, label) = when(t);
        out.push(Alert {
            priority: Priority::P2,
            key: format!("fc-storm:{day}"),
            level: 1,
            text: format!("📡 预报{label}有雷暴——雷暴前断开天线馈线并接地"),
        });
    }
    if let Some((t, _)) = meteo_ahead
        .iter()
        .filter(|(_, h)| h.hail())
        .min_by_key(|(t, _)| *t)
    {
        let (day, label) = when(*t);
        out.push(Alert {
            priority: Priority::P2,
            key: format!("fc-hail:{day}"),
            level: 1,
            text: format!("📡 预报{label}可能有冰雹——收起或遮挡易损天线"),
        });
    }
    out
}

/// BOM warning type → (priority, Chinese name). Marine and other warnings
/// that do not concern the house are ignored.
fn warning_kind(w: &Warning) -> Option<(Priority, &'static str)> {
    let title = w.title.to_ascii_lowercase();
    match w.kind.as_str() {
        "severe_thunderstorm_warning" => Some((Priority::P1, "强雷暴预警")),
        "severe_weather_warning" => Some((Priority::P1, "恶劣天气预警")),
        "flood_warning" | "flood_watch" => Some((Priority::P2, "洪水预警")),
        "fire_weather_warning" => Some((Priority::P2, "火险天气预警")),
        _ if title.contains("severe thunderstorm") => Some((Priority::P1, "强雷暴预警")),
        _ if title.contains("severe weather") => Some((Priority::P1, "恶劣天气预警")),
        _ => None,
    }
}

/// Phenomena named in a warning title, in Chinese, with the antenna advice
/// each one calls for.
fn phenomena(w: &Warning) -> (Vec<&'static str>, Vec<&'static str>) {
    let t = w.title.to_ascii_lowercase();
    let mut what = Vec::new();
    let mut todo = Vec::new();
    if t.contains("destructive wind") {
        what.push("毁灭性大风");
        todo.push("立即放倒或拆下天线");
    } else if t.contains("damaging wind") {
        what.push("破坏性大风");
        todo.push("放倒天线或检查风绳");
    }
    if t.contains("giant hail") {
        what.push("特大冰雹");
        todo.push("收起或遮挡易损天线");
    } else if t.contains("large hail") || t.contains("hail") {
        what.push("大冰雹");
        todo.push("收起或遮挡易损天线");
    }
    if t.contains("intense rain") {
        what.push("特强降雨");
    } else if t.contains("heavy rain") {
        what.push("强降雨");
    }
    if t.contains("tornado") {
        what.push("龙卷风");
    }
    if w.kind == "severe_thunderstorm_warning" || t.contains("thunderstorm") {
        todo.insert(0, "断开馈线并接地");
    }
    (what, todo)
}

/// Official warnings. `was_sent` tells whether a key was already notified,
/// so a cancellation is only reported for a warning that was announced.
pub fn warning_alerts(
    warnings: &[Warning],
    was_sent: impl Fn(&str) -> bool,
    local: &LocalConfig,
) -> Vec<Alert> {
    let tz = local.tz();
    let mut out = Vec::new();
    for w in warnings {
        let Some((priority, name)) = warning_kind(w) else {
            continue;
        };
        let key = format!("bom:{}", w.id);
        if matches!(w.phase.as_str(), "cancelled" | "final" | "expired") {
            if was_sent(&key) {
                out.push(Alert {
                    priority: Priority::P2,
                    key: format!("bom-end:{}", w.id),
                    level: 1,
                    text: format!("✅ BOM {name}已解除"),
                });
            }
            continue;
        }
        let (what, todo) = phenomena(w);
        let mut text = format!("🚨 BOM {name}");
        if !what.is_empty() {
            let _ = write!(text, "：{}", what.join("、"));
        }
        if let Some(exp) = w.expiry {
            let _ = write!(text, "，有效至 {}", exp.with_timezone(&tz).format("%H:%M"));
        }
        if !todo.is_empty() {
            let _ = write!(text, "\n   📡 {}", todo.join("；"));
        }
        out.push(Alert {
            priority,
            key,
            level: 1,
            text,
        });
    }
    out
}

/// Distance from the incident to the nearest of home, school and the route's
/// waypoints.
fn corridor_km(i: &Incident, local: &LocalConfig, via: &[[f64; 2]]) -> f64 {
    let p = (i.lat, i.lon);
    std::iter::once(local.home.at())
        .chain(std::iter::once(local.school.at()))
        .chain(via.iter().map(|v| (v[0], v[1])))
        .map(|q| feeds::km(p, q))
        .fold(f64::INFINITY, f64::min)
}

/// The route an incident sits on, if any: a road of that route, near it.
pub fn route_of<'a>(i: &Incident, local: &'a LocalConfig) -> Option<&'a super::Route> {
    local.commute.routes.iter().find(|r| {
        r.roads
            .iter()
            .any(|road| road.eq_ignore_ascii_case(&i.street))
            && corridor_km(i, local, &r.via) <= local.alerts.route_corridor_km
    })
}

pub fn incident_line(i: &Incident) -> String {
    let mut s = format!("{}：{}", feeds::incident_zh(&i.category), i.street);
    if !i.cross_street.is_empty() {
        let _ = write!(s, " 近 {}", i.cross_street);
    }
    if !i.suburb.is_empty() {
        let _ = write!(s, "（{}）", i.suburb);
    }
    if i.lanes > 0 {
        let _ = write!(s, "，占 {} 条车道", i.lanes);
    }
    s
}

/// Crashes marked major near home, and crashes on the school run during it.
pub fn traffic_alerts(incidents: &[Incident], local: &LocalConfig, school_run: bool) -> Vec<Alert> {
    let mut out = Vec::new();
    for i in incidents {
        let dist = feeds::km(local.home.at(), (i.lat, i.lon));
        if i.is_major && dist <= local.alerts.major_incident_km {
            out.push(Alert {
                priority: Priority::P2,
                key: format!("lt:{}", i.id),
                level: 1,
                text: format!(
                    "⚠️ 附近重大交通事件：{}，距家约 {dist:.1} 公里",
                    incident_line(i)
                ),
            });
            continue;
        }
        if !school_run || !i.category.eq_ignore_ascii_case("CRASH") {
            continue;
        }
        if let Some(route) = route_of(i, local) {
            out.push(Alert {
                priority: Priority::P2,
                key: format!("lt:{}", i.id),
                level: 1,
                text: format!("🚗 送校路线（{}）上有{}", route.name, incident_line(i)),
            });
        }
    }
    out
}

/// RFS fires nearby at Watch and Act or above.
pub fn fire_alerts(fires: &[Fire], local: &LocalConfig) -> Vec<Alert> {
    fires
        .iter()
        .filter_map(|f| {
            let (priority, level, name) = match f.level.as_str() {
                "Emergency Warning" => (Priority::P1, 2, "紧急警告"),
                "Watch and Act" => (Priority::P2, 1, "观察与行动"),
                _ => return None,
            };
            let dist = feeds::km(local.home.at(), (f.lat, f.lon));
            (dist <= local.alerts.fire_km).then(|| {
                let mut text = format!("🔥 山火{name}：{}", f.title);
                if !f.location.is_empty() {
                    let _ = write!(text, "，{}", f.location);
                }
                if !f.status.is_empty() {
                    let _ = write!(text, "，{}", f.status);
                }
                let _ = write!(text, "，距家约 {dist:.0} 公里");
                Alert {
                    priority,
                    key: format!("rfs:{}", f.guid),
                    level,
                    text,
                }
            })
        })
        .collect()
}

pub fn render(alerts: &[Alert], now: DateTime<Utc>, local: &LocalConfig) -> String {
    let urgent = alerts.iter().any(|a| a.priority == Priority::P1);
    let when = now.with_timezone(&local.tz()).format("%m-%d %H:%M");
    let mut out = if urgent {
        format!("🚨 **紧急提醒** | {when}\n")
    } else {
        format!("⚠️ **提醒** | {when}\n")
    };
    let mut sorted: Vec<&Alert> = alerts.iter().collect();
    sorted.sort_by_key(|a| a.priority);
    for a in sorted.iter().take(MAX_LINES) {
        out.push('\n');
        out.push_str(&a.text);
    }
    if sorted.len() > MAX_LINES {
        let _ = write!(out, "\n…另有 {} 条", sorted.len() - MAX_LINES);
    }
    out
}

// ── the job ──────────────────────────────────────────────────────────────

fn due(conn: &rusqlite::Connection, source: &str, every_min: i64, now: DateTime<Utc>) -> bool {
    let last: Option<String> = conn
        .query_row("SELECT at FROM fetched WHERE source = ?1", [source], |r| {
            r.get(0)
        })
        .ok();
    let Some(last) = last.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()) else {
        return true;
    };
    // A minute of slack: the job itself runs every five minutes.
    now - last.with_timezone(&Utc) >= Duration::minutes(every_min - 1)
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let body = crate::cron::news_pipeline::get_text(client, url)
        .await
        .map_err(|e| tracing::warn!(url, "local alert feed failed: {e:#}"))
        .ok()?;
    serde_json::from_str(&body)
        .map_err(|e| tracing::warn!(url, "local alert feed not JSON: {e}"))
        .ok()
}

pub async fn run(config: &Config, local: &LocalConfig) -> Result<String> {
    let now = Utc::now();
    // The connection is not held across the fetches below (it is not `Sync`,
    // and the scheduler needs this future to be `Send`).
    let wanted: Vec<&str> = {
        let conn = open_state(&config.workspace_dir)?;
        CADENCE
            .iter()
            .filter(|(s, m)| due(&conn, s, *m, now))
            .map(|(s, _)| *s)
            .collect()
    };
    if wanted.is_empty() {
        return Ok(NO_REPLY.to_string());
    }
    let client = crate::cron::news_pipeline::http_client()?;
    let gh = &local.home.bom_geohash[..6.min(local.home.bom_geohash.len())];
    let want = |s: &str| wanted.contains(&s);

    let warnings = if want("bom_warnings") {
        get_json(&client, &feeds::bom_url(gh, "warnings")).await
    } else {
        None
    };
    let (hourly, meteo) = if want("forecast") {
        (
            get_json(&client, &feeds::bom_url(gh, "forecasts/hourly")).await,
            get_json(
                &client,
                &feeds::open_meteo_url(local.home.lat, local.home.lon, &local.timezone),
            )
            .await,
        )
    } else {
        (None, None)
    };
    let traffic = if want("traffic") {
        super::fetch_incidents(&client).await
    } else {
        None
    };
    let rfs = if want("rfs") {
        get_json(&client, feeds::RFS_MAJOR_INCIDENTS).await
    } else {
        None
    };
    // `false` when the model was needed and failed: "news" then stays due and
    // the same headlines are judged on the next poll.
    let news_ok = if want("news") {
        super::breaking::poll(config, local, &client, now)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("breaking-news poll failed: {e:#}");
                false
            })
    } else {
        false
    };
    let today = now.with_timezone(&local.tz()).date_naive();
    let holidays = if traffic.is_some() {
        super::holidays(
            &config.workspace_dir,
            &client,
            chrono::Datelike::year(&today),
        )
        .await
    } else {
        Vec::new()
    };

    let conn = open_state(&config.workspace_dir)?;
    let level_of = |key: &str| -> i64 {
        conn.query_row("SELECT level FROM notified WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .unwrap_or(0)
    };
    let mut candidates = Vec::new();
    let mut fetched = Vec::new();
    if let Some(v) = &warnings {
        fetched.push("bom_warnings");
        candidates.extend(warning_alerts(
            &feeds::parse_bom_warnings(v),
            |k| level_of(k) > 0,
            local,
        ));
    }
    if hourly.is_some() || meteo.is_some() {
        fetched.push("forecast");
        let bom = hourly
            .as_ref()
            .map(feeds::parse_bom_hourly)
            .unwrap_or_default();
        let om = meteo
            .as_ref()
            .map(feeds::parse_open_meteo)
            .unwrap_or_default();
        candidates.extend(forecast_alerts(&bom, &om, now, local));
    }
    if let Some(v) = &traffic {
        fetched.push("traffic");
        let local_now = now.with_timezone(&local.tz());
        let minute = local_now.hour() * 60 + local_now.minute();
        let school_run = local.is_school_day(today, &holidays)
            && (SCHOOL_RUN_START..SCHOOL_RUN_END).contains(&minute);
        candidates.extend(traffic_alerts(
            &feeds::parse_incidents(v),
            local,
            school_run,
        ));
    }
    if let Some(v) = &rfs {
        fetched.push("rfs");
        candidates.extend(fire_alerts(&feeds::parse_rfs(v), local));
    }
    if news_ok {
        fetched.push("news");
    }
    // Every poll: judged events may have waited out the quiet hours.
    candidates.extend(super::breaking::pending_alerts(&conn, now)?);

    let stamp = now.to_rfc3339();
    for source in fetched {
        conn.execute(
            "INSERT OR REPLACE INTO fetched (source, at) VALUES (?1, ?2)",
            rusqlite::params![source, stamp],
        )?;
    }
    let quiet = local.is_quiet(now);
    let mut send = Vec::new();
    for a in candidates {
        if level_of(&a.key) >= a.level {
            continue;
        }
        // Not marked: the first poll after the quiet window re-evaluates it.
        if quiet && a.priority == Priority::P2 {
            continue;
        }
        conn.execute(
            "INSERT OR REPLACE INTO notified (key, level, at) VALUES (?1, ?2, ?3)",
            rusqlite::params![a.key, a.level, stamp],
        )?;
        send.push(a);
    }
    // Keep the table small: nothing older than two weeks matters.
    conn.execute(
        "DELETE FROM notified WHERE at < ?1",
        [(now - Duration::days(14)).to_rfc3339()],
    )?;
    if send.is_empty() {
        return Ok(NO_REPLY.to_string());
    }
    Ok(render(&send, now, local))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample;
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn hour(t: &str, icon: &str, gust: f64) -> HourForecast {
        HourForecast {
            time: at(t),
            icon: icon.into(),
            gust_kmh: Some(gust),
            rain_chance: None,
            rain_max: None,
        }
    }

    #[test]
    fn gusts_map_to_the_two_antenna_thresholds() {
        let l = sample();
        let now = at("2026-09-27T02:00:00Z"); // 12:00 Sydney
        let calm = [hour("2026-09-27T05:00:00Z", "cloudy", 45.0)];
        assert!(forecast_alerts(&calm, &[], now, &l).is_empty());

        let check = [hour("2026-09-27T05:00:00Z", "windy", 65.0)];
        let a = forecast_alerts(&check, &[], now, &l);
        assert_eq!((a[0].level, a[0].priority), (1, Priority::P2));
        assert!(
            a[0].text
                .contains("今天 15 点前后（约 3 小时后）阵风 65 km/h"),
            "{}",
            a[0].text
        );
        assert!(a[0].text.contains("检查天线风绳"));

        let lower = [hour("2026-09-27T10:00:00Z", "windy", 85.0)];
        let a = forecast_alerts(&lower, &[], now, &l);
        assert_eq!(a[0].level, 2);
        assert!(a[0].text.contains("放倒或拆下天线"));
        assert_eq!(a[0].key, "fc-gust:2026-09-27");
    }

    #[test]
    fn only_the_next_day_is_scanned_and_the_past_is_ignored() {
        let l = sample();
        let now = at("2026-09-27T02:00:00Z");
        let far = [
            hour("2026-09-26T20:00:00Z", "windy", 99.0), // already past
            hour("2026-09-28T06:00:00Z", "windy", 99.0), // beyond 24 h
        ];
        assert!(forecast_alerts(&far, &[], now, &l).is_empty());
    }

    #[test]
    fn storms_come_from_either_source_and_hail_from_open_meteo() {
        let l = sample();
        let now = at("2026-09-27T02:00:00Z");
        let bom = [hour("2026-09-27T08:00:00Z", "storm", 30.0)];
        let meteo = [MeteoHour {
            time: chrono::NaiveDateTime::parse_from_str("2026-09-27T20:00", "%Y-%m-%dT%H:%M")
                .unwrap(),
            code: 96,
            gust_kmh: Some(40.0),
        }];
        let a = forecast_alerts(&bom, &meteo, now, &l);
        let storm = a.iter().find(|x| x.key.starts_with("fc-storm")).unwrap();
        assert!(
            storm.text.contains("今天 18 点前后"),
            "earliest storm wins: {}",
            storm.text
        );
        assert!(storm.text.contains("断开天线馈线并接地"));
        let hail = a.iter().find(|x| x.key.starts_with("fc-hail")).unwrap();
        assert!(hail.text.contains("冰雹") && hail.text.contains("今天 20 点前后"));
    }

    #[test]
    fn open_meteo_gusts_stand_in_only_when_bom_has_none() {
        let l = sample();
        let now = at("2026-09-27T02:00:00Z");
        let meteo = [MeteoHour {
            time: chrono::NaiveDateTime::parse_from_str("2026-09-27T16:00", "%Y-%m-%dT%H:%M")
                .unwrap(),
            code: 3,
            gust_kmh: Some(70.0),
        }];
        assert_eq!(forecast_alerts(&[], &meteo, now, &l)[0].level, 1);
        let bom = [hour("2026-09-27T06:00:00Z", "cloudy", 40.0)];
        assert!(
            forecast_alerts(&bom, &meteo, now, &l).is_empty(),
            "BOM is the official forecast; its calm reading wins"
        );
    }

    fn warning(kind: &str, title: &str, phase: &str) -> Warning {
        Warning {
            id: "NSW_TS001".into(),
            kind: kind.into(),
            title: title.into(),
            phase: phase.into(),
            expiry: Some(at("2026-09-27T09:30:00Z")),
        }
    }

    #[test]
    fn a_severe_thunderstorm_warning_is_p1_with_antenna_steps() {
        let l = sample();
        let w = warning(
            "severe_thunderstorm_warning",
            "Severe Thunderstorm Warning for damaging winds, large hailstones and heavy rainfall",
            "new",
        );
        let a = warning_alerts(&[w], |_| false, &l);
        assert_eq!(a[0].priority, Priority::P1);
        assert!(
            a[0].text
                .contains("强雷暴预警：破坏性大风、大冰雹、强降雨，有效至 19:30"),
            "{}",
            a[0].text
        );
        assert!(
            a[0].text
                .contains("断开馈线并接地；放倒天线或检查风绳；收起或遮挡易损天线"),
            "{}",
            a[0].text
        );
    }

    #[test]
    fn marine_warnings_are_ignored_and_cancellations_follow_announcements() {
        let l = sample();
        let marine = warning(
            "marine_wind_warning",
            "Marine Wind Warning for New South Wales",
            "new",
        );
        assert!(warning_alerts(&[marine], |_| false, &l).is_empty());

        let over = warning(
            "severe_weather_warning",
            "Severe Weather Warning for damaging winds",
            "cancelled",
        );
        assert!(
            warning_alerts(std::slice::from_ref(&over), |_| false, &l).is_empty(),
            "never announced"
        );
        let a = warning_alerts(&[over], |k| k == "bom:NSW_TS001", &l);
        assert!(a[0].text.contains("已解除"));
        assert_eq!(a[0].priority, Priority::P2);
    }

    fn incident(id: &str, cat: &str, major: bool, street: &str, lat: f64, lon: f64) -> Incident {
        Incident {
            id: id.into(),
            category: cat.into(),
            is_major: major,
            headline: String::new(),
            street: street.into(),
            cross_street: "Beta Street".into(),
            suburb: "Testville".into(),
            lanes: 1,
            lat,
            lon,
        }
    }

    #[test]
    fn route_crashes_count_only_during_the_school_run() {
        let l = sample();
        // On "Alpha Road" next to route A's waypoint.
        let crash = incident("1", "CRASH", false, "Alpha Road", -33.8405, 150.9645);
        assert!(traffic_alerts(std::slice::from_ref(&crash), &l, false).is_empty());
        let a = traffic_alerts(&[crash], &l, true);
        assert!(
            a[0].text.contains(
                "送校路线（A）上有车祸：Alpha Road 近 Beta Street（Testville），占 1 条车道"
            ),
            "{}",
            a[0].text
        );
    }

    #[test]
    fn a_same_named_road_far_away_is_not_on_the_route() {
        let l = sample();
        // "Alpha Road" but ~8 km away, outside the 2 km corridor.
        let far = incident("2", "CRASH", false, "Alpha Road", -33.78, 150.913);
        assert!(traffic_alerts(&[far], &l, true).is_empty());
    }

    #[test]
    fn a_major_crash_near_home_is_reported_any_time() {
        let l = sample();
        let major = incident("3", "CRASH", true, "Delta Road", -33.818, 150.963);
        let a = traffic_alerts(&[major], &l, false);
        assert!(a[0]
            .text
            .starts_with("⚠️ 附近重大交通事件：车祸：Delta Road"));
        assert!(a[0].text.contains("距家约"));
        let far_major = incident("4", "CRASH", true, "M1", -33.4, 151.163);
        assert!(traffic_alerts(&[far_major], &l, false).is_empty());
    }

    #[test]
    fn fires_escalate_from_watch_and_act_to_emergency() {
        let l = sample();
        let mk = |level: &str| Fire {
            guid: "g1".into(),
            title: "Wolli Creek".into(),
            level: level.into(),
            location: "Bardwell Valley".into(),
            status: "Out of control".into(),
            lat: -33.835,
            lon: 150.993,
        };
        assert!(fire_alerts(&[mk("Advice")], &l).is_empty());
        let w = fire_alerts(&[mk("Watch and Act")], &l);
        assert_eq!((w[0].priority, w[0].level), (Priority::P2, 1));
        let e = fire_alerts(&[mk("Emergency Warning")], &l);
        assert_eq!((e[0].priority, e[0].level), (Priority::P1, 2));
        assert_eq!(w[0].key, e[0].key, "same fire, higher level → sent again");
    }

    #[test]
    fn urgent_alerts_lead_the_message() {
        let l = sample();
        let p2 = Alert {
            priority: Priority::P2,
            key: "a".into(),
            level: 1,
            text: "P2 line".into(),
        };
        let p1 = Alert {
            priority: Priority::P1,
            key: "b".into(),
            level: 1,
            text: "P1 line".into(),
        };
        let out = render(&[p2, p1], at("2026-09-27T13:30:00Z"), &l);
        assert!(out.starts_with("🚨 **紧急提醒** | 09-27 23:30"));
        assert!(out.find("P1 line").unwrap() < out.find("P2 line").unwrap());
    }
}
