//! elfClaw 2026-09-27: local weather, school-run traffic and emergency alerts.
//!
//! Two code-run jobs, registered from `workspace/LOCAL.toml`:
//!
//! - `local:早间路况` — every day at the configured time: today's weather,
//!   the school-run hour, and which of the configured routes is quicker right
//!   now (TomTom live traffic, when a key is set) with incidents on each route;
//! - `local:紧急警报` — every five minutes: official BOM warnings, forecast
//!   gusts / storms / hail against the antenna thresholds, major crashes
//!   nearby and on the school run, RFS fires nearby, and breaking news of
//!   attacks and violence (see `breaking`). Nothing new → the job returns
//!   `NO_REPLY` and nothing is sent.
//!
//! Every decision is a rule and every message is a template, with one
//! exception: breaking-news headlines that pass the code's keyword filter
//! are judged by the model (is it a live danger, is it a repeat). The home
//! and school coordinates are personal data, which is why they live in the
//! workspace file and never in code.

pub mod alerts;
pub mod breaking;
pub mod commute;
pub mod feeds;

use crate::config::Config;
use crate::cron::{self, JobType, Schedule};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc, Weekday};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DATA_FILE: &str = "LOCAL.toml";
pub const JOB_PREFIX: &str = "local:";
pub const COMMUTE_JOB: &str = "local:早间路况";
pub const ALERTS_JOB: &str = "local:紧急警报";
const ALERTS_EVERY_MINUTES: u32 = 5;
/// Returned by a job that has nothing to say; the scheduler skips delivery.
pub const NO_REPLY: &str = "NO_REPLY";
/// TomTom key, from the environment like `MONID_API_KEY`.
pub const TOMTOM_KEY_ENV: &str = "TOMTOM_API_KEY";
/// TfNSW Open Data API token, from the environment.
pub const TFNSW_KEY_ENV: &str = "TFNSW_API_KEY";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    pub timezone: String,
    pub home: Place,
    pub school: Place,
    pub commute: Commute,
    #[serde(default, rename = "school_term")]
    pub school_terms: Vec<Term>,
    pub antenna: Antenna,
    pub alerts: Alerts,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Place {
    #[serde(default)]
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    /// BOM location id (six-character geohash); only needed for home.
    #[serde(default)]
    pub bom_geohash: String,
}

impl Place {
    pub fn at(&self) -> (f64, f64) {
        (self.lat, self.lon)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commute {
    /// When the push goes out, `HH:MM`.
    pub time: String,
    /// When the trip starts, `HH:MM` — the time TomTom predicts for.
    pub depart: String,
    /// Route taken when the others are not clearly quicker.
    pub prefer: String,
    /// "Clearly quicker" means by at least this many minutes.
    pub tie_minutes: i64,
    /// Chat IDs that get this push, on the news pushes' channel. Empty: the
    /// news pushes' recipient only. The alerts job always goes to that one.
    #[serde(default)]
    pub send_to: Vec<String>,
    #[serde(rename = "route")]
    pub routes: Vec<Route>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    /// Points the route is forced through, `[lat, lon]`, in order.
    pub via: Vec<[f64; 2]>,
    /// Road names as LiveTraffic spells them ("Example Road").
    pub roads: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Term {
    pub start: NaiveDate,
    pub end: NaiveDate,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Antenna {
    /// Forecast gust at which to check the guy wires.
    pub check_gust_kmh: f64,
    /// Forecast gust at which to lower or take down the antennas.
    pub lower_gust_kmh: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    /// P2 alerts wait out this window; P1 alerts do not.
    pub quiet_start: String,
    pub quiet_end: String,
    /// A crash LiveTraffic marks as major, within this distance of home.
    pub major_incident_km: f64,
    /// An RFS fire at Watch and Act or above, within this distance of home.
    pub fire_km: f64,
    /// An incident counts as "on the school run" when it is on a route road
    /// and within this distance of home, school or a route waypoint.
    pub route_corridor_km: f64,
    /// Suburbs around home, as news headlines spell them. Violence here is
    /// worth a message even when it is over; elsewhere in Sydney only a live
    /// danger is.
    #[serde(default)]
    pub nearby_suburbs: Vec<String>,
}

pub fn data_path(config: &Config) -> PathBuf {
    config.workspace_dir.join(DATA_FILE)
}

/// `Ok(None)` when the file does not exist — the feature is simply off.
pub fn load(config: &Config) -> Result<Option<LocalConfig>> {
    let path = data_path(config);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("读取 {} 失败", path.display()))?;
    let local: LocalConfig =
        toml::from_str(&raw).with_context(|| format!("{DATA_FILE} 格式错误"))?;
    validate(&local)?;
    Ok(Some(local))
}

fn validate(l: &LocalConfig) -> Result<()> {
    if l.timezone.parse::<chrono_tz::Tz>().is_err() {
        bail!("{DATA_FILE}: timezone '{}' 不是有效的时区名", l.timezone);
    }
    for (name, v) in [
        ("commute.time", &l.commute.time),
        ("commute.depart", &l.commute.depart),
        ("alerts.quiet_start", &l.alerts.quiet_start),
        ("alerts.quiet_end", &l.alerts.quiet_end),
    ] {
        if crate::config::parse_hhmm(v).is_none() {
            bail!("{DATA_FILE}: {name} '{v}' 不是 HH:MM");
        }
    }
    if l.commute.routes.is_empty() {
        bail!("{DATA_FILE}: 至少要有一条 [[commute.route]]");
    }
    if !l.commute.routes.iter().any(|r| r.name == l.commute.prefer) {
        bail!(
            "{DATA_FILE}: commute.prefer '{}' 不是任何一条路线的名字",
            l.commute.prefer
        );
    }
    if let Some(bad) = l
        .commute
        .send_to
        .iter()
        .find(|t| t.trim().is_empty() || t.contains(','))
    {
        bail!("{DATA_FILE}: commute.send_to 里的 '{bad}' 不是有效的聊天 ID");
    }
    if l.home.bom_geohash.trim().len() < 6 {
        bail!("{DATA_FILE}: home.bom_geohash 需要至少 6 位（BOM 地点编号）");
    }
    if l.antenna.lower_gust_kmh <= l.antenna.check_gust_kmh {
        bail!("{DATA_FILE}: antenna.lower_gust_kmh 要大于 check_gust_kmh");
    }
    Ok(())
}

impl LocalConfig {
    pub fn tz(&self) -> chrono_tz::Tz {
        self.timezone
            .parse()
            .unwrap_or(chrono_tz::Australia::Sydney)
    }

    /// Weekday, inside a school term (when terms are configured), not a
    /// public holiday.
    pub fn is_school_day(&self, day: NaiveDate, holidays: &[NaiveDate]) -> bool {
        if matches!(day.weekday(), Weekday::Sat | Weekday::Sun) || holidays.contains(&day) {
            return false;
        }
        self.school_terms.is_empty()
            || self
                .school_terms
                .iter()
                .any(|t| t.start <= day && day <= t.end)
    }

    /// Inside the quiet window (which may span midnight).
    pub fn is_quiet(&self, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&self.tz());
        let minutes = chrono::Timelike::hour(&local) * 60 + chrono::Timelike::minute(&local);
        let (Some(start), Some(end)) = (
            crate::config::parse_hhmm(&self.alerts.quiet_start),
            crate::config::parse_hhmm(&self.alerts.quiet_end),
        ) else {
            return false;
        };
        if start <= end {
            (start..end).contains(&minutes)
        } else {
            minutes >= start || minutes < end
        }
    }
}

// ── state ────────────────────────────────────────────────────────────────

pub(crate) fn open_state(workspace: &Path) -> Result<rusqlite::Connection> {
    let path = workspace.join("state").join("alerts.db");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS notified (
             key TEXT PRIMARY KEY,
             level INTEGER NOT NULL,
             at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS fetched (
             source TEXT PRIMARY KEY,
             at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS commute_history (
             day TEXT NOT NULL,
             route TEXT NOT NULL,
             seconds INTEGER NOT NULL,
             PRIMARY KEY (day, route)
         );
         CREATE TABLE IF NOT EXISTS holidays (
             year INTEGER PRIMARY KEY,
             dates TEXT NOT NULL
         );",
    )?;
    breaking::ensure_tables(&conn)?;
    Ok(conn)
}

/// NSW public holidays for `year`, fetched once and kept. A failed fetch
/// yields no holidays (the push then goes out, which is the harmless side).
pub(crate) async fn holidays(
    conn_path: &Path,
    client: &reqwest::Client,
    year: i32,
) -> Vec<NaiveDate> {
    let cached: Option<String> = open_state(conn_path).ok().and_then(|c| {
        c.query_row("SELECT dates FROM holidays WHERE year = ?1", [year], |r| {
            r.get(0)
        })
        .ok()
    });
    let parse = |s: &str| -> Vec<NaiveDate> {
        s.split(',')
            .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .collect()
    };
    if let Some(dates) = cached {
        return parse(&dates);
    }
    let url = format!("{}/{year}/AU", feeds::HOLIDAYS_API);
    let Ok(body) = crate::cron::news_pipeline::get_text(client, &url).await else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Vec::new();
    };
    let dates = feeds::parse_nsw_holidays(&v);
    let joined = dates
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    if let Ok(c) = open_state(conn_path) {
        let _ = c.execute(
            "INSERT OR REPLACE INTO holidays (year, dates) VALUES (?1, ?2)",
            rusqlite::params![year, joined],
        );
    }
    dates
}

/// Current traffic incidents: the official TfNSW API when a token is set,
/// otherwise (or if that fails) the LiveTraffic site's own feed. Both return
/// the same GeoJSON, so one parser serves both.
pub(crate) async fn fetch_incidents(client: &reqwest::Client) -> Option<serde_json::Value> {
    let token = std::env::var(TFNSW_KEY_ENV).unwrap_or_default();
    if !token.trim().is_empty() {
        let official = client
            .get(feeds::TFNSW_INCIDENTS)
            .header("Authorization", format!("apikey {}", token.trim()))
            .header("Accept", "application/json")
            .send()
            .await;
        match official {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    return Some(v);
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "TfNSW hazards API refused; using the public feed");
            }
            Err(e) => tracing::warn!("TfNSW hazards API failed: {e}; using the public feed"),
        }
    }
    let body = crate::cron::news_pipeline::get_text(client, feeds::LIVETRAFFIC_INCIDENTS)
        .await
        .map_err(|e| tracing::warn!("LiveTraffic feed failed: {e:#}"))
        .ok()?;
    serde_json::from_str(&body).ok()
}

// ── jobs ─────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ReconcileReport {
    pub created: Vec<String>,
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

/// The morning push goes to `send_to` when set (the scheduler delivers a
/// comma-separated `to` to each target), otherwise where the news goes.
fn commute_delivery(news: &cron::DeliveryConfig, send_to: &[String]) -> cron::DeliveryConfig {
    let targets: Vec<&str> = send_to
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if targets.is_empty() {
        return news.clone();
    }
    cron::DeliveryConfig {
        to: Some(targets.join(",")),
        ..news.clone()
    }
}

/// Every day, holidays included (the user's call, 2026-09-28): the weather
/// matters on any day, and the route times are still worth a glance.
fn daily_cron(hhmm: &str) -> Option<String> {
    let m = crate::config::parse_hhmm(hhmm)?;
    Some(format!("{} {} * * *", m % 60, m / 60))
}

/// Create, update or remove the `local:` jobs to match `LOCAL.toml`. No file
/// → the jobs are removed; a file that does not parse → nothing changes.
pub fn reconcile(config: &Config) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let local = match load(config) {
        Ok(l) => l,
        Err(e) => {
            report.errors.push(format!("{e:#}"));
            return Ok(report);
        }
    };
    let Some(local) = local else {
        for job in cron::list_jobs(config)? {
            if job
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(JOB_PREFIX))
            {
                cron::remove_job(config, &job.id)?;
                report.removed.push(job.name.unwrap_or_default());
            }
        }
        return Ok(report);
    };
    // Same recipient as the news pushes.
    let delivery = match crate::cron::news::load_rules(config) {
        Ok(rules) => rules.delivery,
        Err(e) => {
            report.errors.push(format!(
                "读取推送对象失败（HEARTBEAT.md news-rules）: {e:#}"
            ));
            return Ok(report);
        }
    };
    let commute_delivery = commute_delivery(&delivery, &local.commute.send_to);
    let tz = Some(local.timezone.clone());
    let wanted = [
        (
            COMMUTE_JOB,
            "commute",
            Schedule::Cron {
                expr: daily_cron(&local.commute.time).unwrap_or_else(|| "15 8 * * *".into()),
                tz: tz.clone(),
            },
            commute_delivery,
        ),
        (
            ALERTS_JOB,
            "alerts",
            Schedule::Cron {
                expr: format!("*/{ALERTS_EVERY_MINUTES} * * * *"),
                tz,
            },
            delivery,
        ),
    ];
    for (name, task, schedule, delivery) in wanted {
        if let Some(job) = cron::find_job_by_name(config, name)? {
            let same = job.job_type == JobType::Local
                && job.schedule == schedule
                && job.prompt.as_deref() == Some(task)
                && job.delivery == delivery;
            if same {
                continue;
            }
        }
        cron::add_local_job(
            config,
            Some(name.to_string()),
            schedule,
            task,
            Some(delivery.clone()),
        )?;
        report.created.push(name.to_string());
    }
    Ok(report)
}

/// Run one `local:` job. Returns the message, or `NO_REPLY` for silence.
pub async fn run(config: &Config, task: &str) -> Result<String> {
    let Some(local) = load(config)? else {
        return Ok(NO_REPLY.to_string());
    };
    match task {
        "commute" => Box::pin(commute::run(config, &local)).await,
        "alerts" => Box::pin(alerts::run(config, &local)).await,
        other => bail!("未知的本地任务 '{other}'"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = r#"
timezone = "Australia/Sydney"

[home]
lat = -33.8497
lon = 150.9447
bom_geohash = "r0test"

[school]
name = "Test School"
lat = -33.8245
lon = 151.0014

[commute]
time = "08:15"
depart = "08:25"
prefer = "A"
tie_minutes = 2

[[commute.route]]
name = "A"
via = [[-33.841, 150.964]]
roads = ["Beta Street", "Alpha Road"]

[[commute.route]]
name = "B"
via = [[-33.838, 150.970]]
roads = ["Gamma Road", "Alpha Road"]
note = "经过学校区"

[[school_term]]
start = "2026-10-13"
end = "2026-12-17"

[antenna]
check_gust_kmh = 60
lower_gust_kmh = 80

[alerts]
quiet_start = "23:00"
quiet_end = "06:30"
major_incident_km = 10
fire_km = 20
route_corridor_km = 2
"#;

    pub(crate) fn sample() -> LocalConfig {
        let l: LocalConfig = toml::from_str(SAMPLE).unwrap();
        validate(&l).unwrap();
        l
    }

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn school_days_are_term_weekdays_that_are_not_holidays() {
        let l = sample();
        assert!(l.is_school_day(d("2026-10-13"), &[]), "first day of term 4");
        assert!(
            !l.is_school_day(d("2026-10-12"), &[]),
            "development day, before the term"
        );
        assert!(!l.is_school_day(d("2026-10-17"), &[]), "Saturday");
        assert!(!l.is_school_day(d("2026-09-28"), &[]), "school holidays");
        assert!(
            !l.is_school_day(d("2026-10-14"), &[d("2026-10-14")]),
            "public holiday"
        );
        let mut open = sample();
        open.school_terms.clear();
        assert!(
            open.is_school_day(d("2026-09-28"), &[]),
            "no terms configured → every weekday"
        );
    }

    #[test]
    fn the_quiet_window_spans_midnight() {
        let l = sample();
        // Sydney is UTC+10 in late September.
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert!(l.is_quiet(at("2026-09-27T13:30:00Z")), "23:30");
        assert!(l.is_quiet(at("2026-09-27T16:00:00Z")), "02:00");
        assert!(!l.is_quiet(at("2026-09-27T20:30:00Z")), "06:30 is the end");
        assert!(!l.is_quiet(at("2026-09-27T02:00:00Z")), "12:00");
    }

    #[test]
    fn validation_rejects_the_obvious_mistakes() {
        let bad = SAMPLE.replace("prefer = \"A\"", "prefer = \"C\"");
        let l: LocalConfig = toml::from_str(&bad).unwrap();
        assert!(validate(&l).is_err(), "prefer must name a route");
        let bad = SAMPLE.replace("lower_gust_kmh = 80", "lower_gust_kmh = 50");
        let l: LocalConfig = toml::from_str(&bad).unwrap();
        assert!(validate(&l).is_err(), "lower must exceed check");
        assert!(
            toml::from_str::<LocalConfig>(&SAMPLE.replace("[alerts]", "[alerts]\nextra = 1"))
                .is_err()
        );
    }

    #[test]
    fn commute_push_goes_to_every_listed_chat() {
        let news = cron::DeliveryConfig {
            mode: "announce".into(),
            channel: Some("telegram".into()),
            to: Some("100".into()),
            best_effort: true,
        };
        let d = commute_delivery(&news, &[" 100 ".into(), "200".into()]);
        assert_eq!(d.to.as_deref(), Some("100,200"));
        assert_eq!(d.channel.as_deref(), Some("telegram"));
        assert_eq!(
            commute_delivery(&news, &[]),
            news,
            "empty list: news recipient"
        );

        let mut l = sample();
        l.commute.send_to = vec!["100,200".into()];
        assert!(validate(&l).is_err(), "one ID per entry");
    }

    #[test]
    fn the_commute_job_runs_every_day_at_the_configured_time() {
        assert_eq!(daily_cron("08:15").as_deref(), Some("15 8 * * *"));
        assert_eq!(daily_cron("bad"), None);
    }
}
