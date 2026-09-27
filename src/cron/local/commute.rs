//! The school-day morning push: today's weather, the school-run hour, and
//! which configured route is quicker right now. Pure rules, no model.
//!
//! Travel times come from TomTom with live traffic (key in `TOMTOM_API_KEY`),
//! each route forced through its own waypoints and predicted for the departure
//! time. Without a key the push still lists incidents on each route. Every
//! day's times are kept, so the push can say how today compares with usual.

use super::feeds::{self, DayForecast, HourForecast, Incident, RouteEta};
use super::{open_state, LocalConfig, NO_REPLY, TOMTOM_KEY_ENV};
use crate::config::Config;
use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Timelike, Utc};
use std::fmt::Write as _;

/// Past days of a route's times used for "usual".
const BASELINE_DAYS: usize = 20;
/// Fewer samples than this and no comparison with usual is made.
const MIN_BASELINE: usize = 3;
/// Rain chance at which the push says to take an umbrella.
const UMBRELLA_CHANCE: f64 = 40.0;
const WEEKDAYS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 0.05 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// "今天阵雨，12–19°C，降雨概率 90%（4–6 mm），最大阵风 42 km/h，紫外线 7"
pub fn weather_line(day: &DayForecast, max_gust: Option<f64>) -> String {
    let mut parts = vec![format!("今天{}", feeds::icon_zh(&day.icon))];
    match (day.temp_min, day.temp_max) {
        (Some(lo), Some(hi)) => parts.push(format!("{}–{}°C", fmt_num(lo), fmt_num(hi))),
        (None, Some(hi)) => parts.push(format!("最高 {}°C", fmt_num(hi))),
        (Some(lo), None) => parts.push(format!("最低 {}°C", fmt_num(lo))),
        (None, None) => {}
    }
    if let Some(c) = day.rain_chance {
        let mut s = format!("降雨概率 {}%", fmt_num(c));
        if let (Some(lo), Some(hi)) = (day.rain_min, day.rain_max) {
            if hi > 0.0 {
                let _ = write!(s, "（{}–{} mm）", fmt_num(lo), fmt_num(hi));
            }
        }
        parts.push(s);
    }
    if let Some(g) = max_gust {
        parts.push(format!("最大阵风 {} km/h", fmt_num(g)));
    }
    if let Some(uv) = day.uv_max {
        parts.push(format!("紫外线 {}", fmt_num(uv)));
    }
    parts.join("，")
}

/// Rain during the hour the school run starts in.
pub fn school_run_line(
    hours: &[HourForecast],
    local: &LocalConfig,
    today: NaiveDate,
) -> Option<String> {
    let tz = local.tz();
    let start = crate::config::parse_hhmm(&local.commute.depart)? / 60;
    let chance = hours
        .iter()
        .filter(|h| {
            let l = h.time.with_timezone(&tz);
            l.date_naive() == today && l.hour() == start
        })
        .filter_map(|h| h.rain_chance)
        .fold(None::<f64>, |acc, c| Some(acc.map_or(c, |a| a.max(c))))?;
    let advice = if chance >= UMBRELLA_CHANCE {
        "，记得带伞"
    } else {
        ""
    };
    Some(format!(
        "{} {start} 点到 {} 点降雨概率 {}%{advice}",
        if chance >= UMBRELLA_CHANCE {
            "☔"
        } else {
            "🌂"
        },
        start + 1,
        fmt_num(chance)
    ))
}

/// Today's strongest forecast gust (BOM).
pub fn max_gust_today(
    hours: &[HourForecast],
    local: &LocalConfig,
    today: NaiveDate,
) -> Option<f64> {
    let tz = local.tz();
    hours
        .iter()
        .filter(|h| h.time.with_timezone(&tz).date_naive() == today)
        .filter_map(|h| h.gust_kmh)
        .fold(None, |acc: Option<f64>, g| {
            Some(acc.map_or(g, |a| a.max(g)))
        })
}

/// The route to take: the quickest, unless the preferred one is within
/// `tie_minutes` of it.
pub fn choose<'a>(times: &'a [(String, i64)], prefer: &str, tie_minutes: i64) -> Option<&'a str> {
    let best = times.iter().min_by_key(|(_, s)| *s)?;
    let preferred = times.iter().find(|(n, _)| n == prefer);
    Some(match preferred {
        Some((name, secs)) if secs - best.1 < tie_minutes * 60 => name.as_str(),
        _ => best.0.as_str(),
    })
}

/// Median of past seconds, when there are enough samples to mean anything.
pub fn usual(past: &[i64]) -> Option<i64> {
    if past.len() < MIN_BASELINE {
        return None;
    }
    let mut v = past.to_vec();
    v.sort_unstable();
    Some(v[v.len() / 2])
}

fn minutes(secs: i64) -> i64 {
    (secs + 30) / 60
}

pub struct RouteReport<'a> {
    pub route: &'a super::Route,
    pub eta: Option<RouteEta>,
    pub usual: Option<i64>,
    pub incidents: Vec<&'a Incident>,
}

pub fn render(
    local: &LocalConfig,
    now: DateTime<Utc>,
    weather: Option<String>,
    school_run: Option<String>,
    routes: &[RouteReport<'_>],
    have_key: bool,
) -> String {
    let l = now.with_timezone(&local.tz());
    let mut out = format!(
        "🚸 **送校路况** | {} {} {}\n",
        l.format("%m-%d"),
        WEEKDAYS[l.weekday().num_days_from_monday() as usize],
        l.format("%H:%M")
    );
    if let Some(w) = weather {
        let _ = write!(out, "\n🌤 {w}");
    }
    if let Some(s) = school_run {
        let _ = write!(out, "\n{s}");
    }
    let school = if local.school.name.is_empty() {
        "学校".to_string()
    } else {
        local.school.name.clone()
    };
    let _ = write!(out, "\n\n🚗 {} 出发去 {school}", local.commute.depart);

    let times: Vec<(String, i64)> = routes
        .iter()
        .filter_map(|r| Some((r.route.name.clone(), r.eta.as_ref()?.seconds)))
        .collect();
    let pick = choose(&times, &local.commute.prefer, local.commute.tie_minutes).map(str::to_string);
    for r in routes {
        let mark = if pick.as_deref() == Some(r.route.name.as_str()) {
            "✅ 推荐"
        } else {
            "   "
        };
        let mut line = format!("\n{mark} 走 {}", r.route.name);
        if let Some(eta) = &r.eta {
            let _ = write!(line, "：约 {} 分钟", minutes(eta.seconds));
            let mut extra = Vec::new();
            if eta.delay_seconds >= 60 {
                extra.push(format!("堵车多 {} 分钟", minutes(eta.delay_seconds)));
            }
            if let Some(u) = r.usual {
                let diff = minutes(eta.seconds) - minutes(u);
                if diff >= 1 {
                    extra.push(format!("比平时慢 {diff} 分钟"));
                } else if diff <= -1 {
                    extra.push(format!("比平时快 {} 分钟", -diff));
                }
            }
            if !extra.is_empty() {
                let _ = write!(line, "（{}）", extra.join("，"));
            }
        }
        if !r.route.note.is_empty() {
            let _ = write!(line, " · {}", r.route.note);
        }
        out.push_str(&line);
        for i in &r.incidents {
            let _ = write!(out, "\n   ⚠️ {}", super::alerts::incident_line(i));
        }
    }
    if !have_key {
        out.push_str("\n（还没配置 TomTom key，暂时只看路线上的事故，没法比较用时）");
    } else if times.is_empty() {
        out.push_str("\n（这次没拿到路线用时，只列出路线上的事故）");
    }
    out
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<serde_json::Value> {
    let body = crate::cron::news_pipeline::get_text(client, url)
        .await
        .map_err(|e| tracing::warn!("commute feed failed: {e:#}"))
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// Departure as RFC 3339 in the local zone; "now" once it has passed.
fn departure(local: &LocalConfig, now: DateTime<Utc>) -> String {
    let tz = local.tz();
    let today = now.with_timezone(&tz).date_naive();
    let planned = crate::config::parse_hhmm(&local.commute.depart)
        .and_then(|m| today.and_hms_opt(m / 60, m % 60, 0))
        .and_then(|t| tz.from_local_datetime(&t).earliest())
        .map(|t| t.with_timezone(&Utc))
        .filter(|t| *t > now)
        .unwrap_or(now + chrono::Duration::minutes(1));
    planned.with_timezone(&tz).to_rfc3339()
}

pub async fn run(config: &Config, local: &LocalConfig) -> Result<String> {
    let now = Utc::now();
    let tz = local.tz();
    let today = now.with_timezone(&tz).date_naive();
    let client = crate::cron::news_pipeline::http_client()?;
    let holidays = super::holidays(&config.workspace_dir, &client, today.year()).await;
    if !local.is_school_day(today, &holidays) {
        return Ok(NO_REPLY.to_string());
    }
    let gh = &local.home.bom_geohash[..6.min(local.home.bom_geohash.len())];
    let daily = get_json(&client, &feeds::bom_url(gh, "forecasts/daily"))
        .await
        .as_ref()
        .and_then(feeds::parse_bom_daily);
    let hourly = get_json(&client, &feeds::bom_url(gh, "forecasts/hourly"))
        .await
        .as_ref()
        .map(feeds::parse_bom_hourly)
        .unwrap_or_default();
    let incidents = super::fetch_incidents(&client)
        .await
        .as_ref()
        .map(feeds::parse_incidents)
        .unwrap_or_default();

    let key = std::env::var(TOMTOM_KEY_ENV).unwrap_or_default();
    let have_key = !key.trim().is_empty();
    let depart = departure(local, now);
    let mut etas = Vec::new();
    for r in &local.commute.routes {
        let eta = if have_key {
            let points: Vec<(f64, f64)> = std::iter::once(local.home.at())
                .chain(r.via.iter().map(|v| (v[0], v[1])))
                .chain(std::iter::once(local.school.at()))
                .collect();
            get_json(&client, &feeds::tomtom_url(key.trim(), &points, &depart))
                .await
                .as_ref()
                .and_then(feeds::parse_tomtom)
        } else {
            None
        };
        etas.push(eta);
    }

    // Sync section: history in, baselines out.
    let conn = open_state(&config.workspace_dir)?;
    let day = today.to_string();
    let mut reports = Vec::new();
    for (r, eta) in local.commute.routes.iter().zip(etas) {
        let past: Vec<i64> = conn
            .prepare(
                "SELECT seconds FROM commute_history WHERE route = ?1 AND day < ?2
                 ORDER BY day DESC LIMIT ?3",
            )?
            .query_map(
                rusqlite::params![r.name, day, BASELINE_DAYS as i64],
                |row| row.get(0),
            )?
            .filter_map(std::result::Result::ok)
            .collect();
        if let Some(e) = &eta {
            conn.execute(
                "INSERT OR REPLACE INTO commute_history (day, route, seconds) VALUES (?1, ?2, ?3)",
                rusqlite::params![day, r.name, e.seconds],
            )?;
        }
        let on_route = incidents
            .iter()
            .filter(|i| super::alerts::route_of(i, local).is_some_and(|x| x.name == r.name))
            .collect();
        reports.push(RouteReport {
            route: r,
            eta,
            usual: usual(&past),
            incidents: on_route,
        });
    }
    let weather = daily
        .as_ref()
        .map(|d| weather_line(d, max_gust_today(&hourly, local, today)));
    let school_run = school_run_line(&hourly, local, today);
    Ok(render(local, now, weather, school_run, &reports, have_key))
}

#[cfg(test)]
mod tests {
    use super::super::tests::sample;
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn weather_line_reads_naturally_with_or_without_a_minimum() {
        let d = DayForecast {
            icon: "shower".into(),
            temp_min: Some(12.0),
            temp_max: Some(19.0),
            rain_chance: Some(90.0),
            rain_min: Some(4.0),
            rain_max: Some(6.0),
            uv_max: Some(7.0),
        };
        assert_eq!(
            weather_line(&d, Some(42.0)),
            "今天阵雨，12–19°C，降雨概率 90%（4–6 mm），最大阵风 42 km/h，紫外线 7"
        );
        let later = DayForecast {
            temp_min: None,
            rain_max: Some(0.0),
            ..d
        };
        assert_eq!(
            weather_line(&later, None),
            "今天阵雨，最高 19°C，降雨概率 90%，紫外线 7"
        );
    }

    #[test]
    fn the_school_run_hour_decides_the_umbrella() {
        let l = sample();
        let today = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        let h = |t: &str, c: f64| HourForecast {
            time: at(t),
            icon: String::new(),
            gust_kmh: None,
            rain_chance: Some(c),
            rain_max: None,
        };
        // 08:00 Sydney (AEDT, UTC+11 from October) = 21:00 UTC the day before.
        let wet = [
            h("2026-10-12T21:00:00Z", 70.0),
            h("2026-10-12T22:00:00Z", 10.0),
        ];
        assert_eq!(
            school_run_line(&wet, &l, today).as_deref(),
            Some("☔ 8 点到 9 点降雨概率 70%，记得带伞")
        );
        let dry = [h("2026-10-12T21:00:00Z", 10.0)];
        assert_eq!(
            school_run_line(&dry, &l, today).as_deref(),
            Some("🌂 8 点到 9 点降雨概率 10%")
        );
        assert_eq!(school_run_line(&[], &l, today), None);
    }

    #[test]
    fn a_small_gap_keeps_the_preferred_route() {
        let t = vec![("A".to_string(), 12 * 60), ("B".to_string(), 11 * 60)];
        assert_eq!(choose(&t, "A", 2), Some("A"), "B saves only a minute");
        let t = vec![("A".to_string(), 18 * 60), ("B".to_string(), 13 * 60)];
        assert_eq!(choose(&t, "A", 2), Some("B"), "B saves five minutes");
        assert_eq!(choose(&[], "A", 2), None);
    }

    #[test]
    fn usual_needs_a_few_days_first() {
        assert_eq!(usual(&[700, 720]), None);
        assert_eq!(usual(&[700, 760, 720, 680, 900]), Some(720));
    }

    #[test]
    fn the_push_marks_the_pick_and_explains_each_number() {
        let l = sample();
        let crash = Incident {
            id: "1".into(),
            category: "CRASH".into(),
            is_major: false,
            headline: String::new(),
            street: "Alpha Road".into(),
            cross_street: "Gamma Road".into(),
            suburb: "Testville".into(),
            lanes: 1,
            lat: -33.84,
            lon: 150.963,
        };
        let routes = [
            RouteReport {
                route: &l.commute.routes[0],
                eta: Some(RouteEta {
                    seconds: 12 * 60,
                    delay_seconds: 2 * 60,
                    length_m: 6900,
                }),
                usual: Some(11 * 60),
                incidents: vec![],
            },
            RouteReport {
                route: &l.commute.routes[1],
                eta: Some(RouteEta {
                    seconds: 17 * 60,
                    delay_seconds: 6 * 60,
                    length_m: 7300,
                }),
                usual: None,
                incidents: vec![&crash],
            },
        ];
        let out = render(
            &l,
            at("2026-10-12T21:15:00Z"),
            Some("今天阵雨".into()),
            Some("☔ 8 点到 9 点降雨概率 70%，记得带伞".into()),
            &routes,
            true,
        );
        assert!(
            out.starts_with("🚸 **送校路况** | 10-13 周二 08:15"),
            "{out}"
        );
        assert!(
            out.contains("✅ 推荐 走 A：约 12 分钟（堵车多 2 分钟，比平时慢 1 分钟）"),
            "{out}"
        );
        assert!(
            out.contains("    走 B：约 17 分钟（堵车多 6 分钟） · 经过学校区"),
            "{out}"
        );
        assert!(
            out.contains("   ⚠️ 车祸：Alpha Road 近 Gamma Road（Testville），占 1 条车道"),
            "{out}"
        );
        assert!(!out.contains("TomTom"), "{out}");
    }

    #[test]
    fn without_a_key_the_push_says_why_there_are_no_times() {
        let l = sample();
        let routes = [RouteReport {
            route: &l.commute.routes[0],
            eta: None,
            usual: None,
            incidents: vec![],
        }];
        let out = render(&l, at("2026-10-12T21:15:00Z"), None, None, &routes, false);
        assert!(out.contains("还没配置 TomTom key"), "{out}");
        assert!(
            !out.contains("✅"),
            "nothing to recommend without times: {out}"
        );
    }
}
