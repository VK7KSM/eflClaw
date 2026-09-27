//! Breaking news that can hurt: terror attacks, active shooters, sieges,
//! explosions, and shootings or stabbings in the suburbs around home.
//!
//! Code does the cheap, deterministic part: fetch four Sydney news feeds,
//! keep headlines that are recent, unseen and contain a danger word. Only
//! when something survives that filter is the model asked, once per poll,
//! to judge it: is this a live danger, which earlier event is it a repeat
//! of, and one line of Chinese saying what happened where. Most polls end
//! at the keyword filter and cost nothing.
//!
//! Judged events are kept in `news_events`; the alert job turns them into
//! ordinary alerts, so de-duplication by level and the quiet hours work the
//! same way as for weather and traffic.

use super::alerts::{Alert, Priority};
use super::LocalConfig;
use crate::cron::news_pipeline::{self, Item};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt::Write as _;

/// Sydney news feeds, measured 2026-09-27. 7NEWS Sydney is the fastest and
/// Sydney-only; ABC "Just In" and SMH are national and so need a place name;
/// the Google News search catches outlets without a usable feed.
pub const FEEDS: &[(&str, &str)] = &[
    ("7NEWS", "https://7news.com.au/news/sydney/feed"),
    ("ABC", "https://www.abc.net.au/news/feed/51120/rss.xml"),
    ("SMH", "https://www.smh.com.au/rss/feed.xml"),
    (
        "Google News",
        "https://news.google.com/rss/search?q=Sydney+(terror+OR+shooting+OR+gunman+OR+stabbing+OR+explosion+OR+siege+OR+hostage+OR+lockdown)+when:1d&hl=en-AU&gl=AU&ceid=AU:en",
    ),
];

/// Feeds whose every item is already about Sydney.
const SYDNEY_ONLY: &[&str] = &["7NEWS", "Google News"];

/// Older headlines are history, not a warning.
const MAX_AGE_HOURS: i64 = 3;
/// A judged event is sent within this window (it may wait out the quiet hours).
const EVENT_TTL_HOURS: i64 = 12;
/// Earlier events shown to the model for de-duplication.
const RECENT_EVENT_HOURS: i64 = 48;
/// Headlines per model call, newest first.
const MAX_CANDIDATES: usize = 30;

/// Lower-case fragments that make a headline worth a second look. Broad on
/// purpose: the model throws out the sport and the court reports.
const DANGER_WORDS: &[&str] = &[
    "terror",
    "gunman",
    "gunmen",
    "gunfire",
    "shooting",
    "shot dead",
    "shots fired",
    "shot in",
    "active shooter",
    "armed offender",
    "stabbing",
    "stabbed",
    "machete",
    "explosion",
    "explosive",
    "bomb",
    "blast",
    "siege",
    "hostage",
    "lockdown",
    "locked down",
    "evacuat",
    "mass casualty",
    "rampage",
    "ramming",
    "firebomb",
    "critical incident",
    "emergency warning",
];

pub fn is_danger(text: &str) -> bool {
    let t = text.to_lowercase();
    DANGER_WORDS.iter().any(|w| t.contains(w))
}

pub fn mentions_place(text: &str, nearby: &[String]) -> bool {
    let t = text.to_lowercase();
    ["sydney", "nsw", "new south wales"]
        .iter()
        .any(|p| t.contains(p))
        || nearby.iter().any(|s| t.contains(&s.to_lowercase()))
}

/// Code-side filter: recent, a danger word, and (for national feeds) a place.
pub fn candidates(
    items: Vec<(&'static str, Item)>,
    seen: &dyn Fn(&str) -> bool,
    nearby: &[String],
    now: DateTime<Utc>,
) -> Vec<(&'static str, Item)> {
    let mut keys = HashSet::new();
    let mut out: Vec<(&'static str, Item)> = items
        .into_iter()
        .filter(|(feed, it)| {
            let fresh = it
                .published
                .is_none_or(|p| now - p <= Duration::hours(MAX_AGE_HOURS));
            let text = format!("{} {}", it.title, it.text);
            fresh
                && is_danger(&text)
                && (SYDNEY_ONLY.contains(feed) || mentions_place(&text, nearby))
                && !seen(&item_key(it))
        })
        .filter(|(_, it)| keys.insert(item_key(it)))
        .collect();
    out.sort_by(|a, b| b.1.published.cmp(&a.1.published));
    out.truncate(MAX_CANDIDATES);
    out
}

pub fn item_key(it: &Item) -> String {
    news_pipeline::url_key(&it.link)
}

// ── the model's part ─────────────────────────────────────────────────────

/// An event already on record, as shown to the model.
#[derive(Debug, Clone)]
pub struct KnownEvent {
    pub key: String,
    pub level: i64,
    pub summary: String,
}

pub const SYSTEM_PROMPT: &str =
    "你是一个悉尼家庭的安全值班员。你只判断新闻标题是否意味着对这家人有现实危险，不做别的。\n\
只输出 JSON，不要任何其他文字。";

pub fn build_request(
    cands: &[(&'static str, Item)],
    known: &[KnownEvent],
    nearby: &[String],
    now: DateTime<Utc>,
    tz: chrono_tz::Tz,
) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "现在是悉尼时间 {}。这家人住在悉尼，家附近的区：{}。\n",
        now.with_timezone(&tz).format("%Y-%m-%d %H:%M"),
        nearby.join(", ")
    );
    s.push_str(
        "把下面的新标题归成事件，每个事件给一个等级：\n\
- P1：悉尼任何地方正在发生或刚刚发生的恐怖袭击、持枪/持刀者仍在逃或仍在行凶、多人伤亡的袭击、人质/围困、大爆炸、公共场所封锁或疏散；以及家附近的区里任何仍有危险的暴力事件。\n\
- P2：家附近的区里已经结束的枪击、持刀伤人、纵火炸弹等严重暴力事件（不影响出行但值得知道）。\n\
- skip：其他一切——悉尼其他区已经结束的个案、法庭审理、逮捕起诉、周年纪念、调查进展、外地或海外新闻、体育娱乐、家暴等不涉及公众的案件。拿不准时选 skip。\n\n\
同一件事的多条标题合成一个事件。如果和下面“已通知过的事件”是同一件事，填 same_as；只有危险升级（例如 P2 变成 P1）才有必要再次通知，其余情况也请照实填 same_as。\n\n",
    );
    if known.is_empty() {
        s.push_str("已通知过的事件：无\n\n");
    } else {
        s.push_str("已通知过的事件：\n");
        for (i, k) in known.iter().enumerate() {
            let lvl = if k.level >= 2 { "P1" } else { "P2" };
            let _ = writeln!(s, "E{} [{lvl}] {}", i + 1, k.summary);
        }
        s.push('\n');
    }
    s.push_str("新标题：\n");
    for (i, (feed, it)) in cands.iter().enumerate() {
        let when = it
            .published
            .map(|p| p.with_timezone(&tz).format("%H:%M").to_string())
            .unwrap_or_default();
        let _ = write!(s, "{i}. [{feed} {when}] {}", it.title);
        let text = it.text.trim();
        // Google News repeats the headline (plus the outlet) as the excerpt.
        let repeats =
            news_pipeline::title_key(text).starts_with(&news_pipeline::title_key(&it.title));
        if !text.is_empty() && !repeats {
            let short: String = text.chars().take(200).collect();
            let _ = write!(s, " —— {short}");
        }
        s.push('\n');
    }
    s.push_str(
        "\n输出格式（只列 P1 和 P2 的事件，skip 的不用列）：\n\
{\"events\":[{\"items\":[0,3],\"level\":\"P1\",\"same_as\":null,\"zh\":\"一句中文：什么事、在哪个区、现在是否仍有危险、对出行有什么影响\"}]}\n\
same_as 填已通知事件的编号（如 \"E2\"）或 null。没有需要通知的事件就输出 {\"events\":[]}。",
    );
    s
}

#[derive(Debug, Deserialize)]
struct Answer {
    events: Vec<AnswerEvent>,
}

#[derive(Debug, Deserialize)]
struct AnswerEvent {
    #[serde(default)]
    items: Vec<usize>,
    level: String,
    #[serde(default)]
    same_as: Option<String>,
    #[serde(default)]
    zh: String,
}

/// One judged event, ready to store.
#[derive(Debug, Clone, PartialEq)]
pub struct Judged {
    pub key: String,
    pub level: i64,
    pub summary: String,
    pub source: String,
    pub link: String,
}

/// Reads the model's answer. `None` means it could not be read (the
/// candidates are then tried again next poll); `Some(vec![])` means the model
/// answered and nothing needs telling.
pub fn parse_answer(
    answer: &str,
    cands: &[(&'static str, Item)],
    known: &[KnownEvent],
) -> Option<Vec<Judged>> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let parsed: Answer = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let mut out = Vec::new();
    for e in parsed.events {
        let level = match e.level.trim().to_ascii_uppercase().as_str() {
            "P1" => 2,
            "P2" => 1,
            _ => continue,
        };
        let summary = e.zh.trim().to_string();
        let Some(first) = e.items.iter().copied().find(|&i| i < cands.len()) else {
            continue;
        };
        if summary.is_empty() {
            continue;
        }
        let known_event = e
            .same_as
            .as_deref()
            .and_then(|s| {
                s.trim()
                    .trim_start_matches(['E', 'e'])
                    .parse::<usize>()
                    .ok()
            })
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| known.get(n));
        let (feed, it) = &cands[first];
        let key = match known_event {
            Some(k) => k.key.clone(),
            None => format!("news:{}", item_key(it)),
        };
        out.push(Judged {
            key,
            level,
            summary,
            source: (*feed).to_string(),
            link: it.link.clone(),
        });
    }
    Some(out)
}

// ── storage ──────────────────────────────────────────────────────────────

pub fn ensure_tables(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS news_seen (
             id TEXT PRIMARY KEY,
             at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS news_events (
             key TEXT PRIMARY KEY,
             level INTEGER NOT NULL,
             summary TEXT NOT NULL,
             source TEXT NOT NULL,
             link TEXT NOT NULL,
             at TEXT NOT NULL
         );",
    )
}

pub fn is_seen(conn: &rusqlite::Connection, id: &str) -> bool {
    conn.query_row("SELECT 1 FROM news_seen WHERE id = ?1", [id], |_| Ok(()))
        .is_ok()
}

pub fn recent_events(
    conn: &rusqlite::Connection,
    now: DateTime<Utc>,
) -> rusqlite::Result<Vec<KnownEvent>> {
    let since = (now - Duration::hours(RECENT_EVENT_HOURS)).to_rfc3339();
    let mut stmt =
        conn.prepare("SELECT key, level, summary FROM news_events WHERE at >= ?1 ORDER BY at")?;
    let rows = stmt.query_map([since], |r| {
        Ok(KnownEvent {
            key: r.get(0)?,
            level: r.get(1)?,
            summary: r.get(2)?,
        })
    })?;
    rows.collect()
}

/// Stores the model's verdicts and marks every candidate as seen. An event
/// that repeats a known one keeps its record unless the danger rose, in
/// which case level, wording and time are replaced (and the alert goes out
/// again, because its level rose).
pub fn record(
    conn: &rusqlite::Connection,
    judged: &[Judged],
    cands: &[(&'static str, Item)],
    now: DateTime<Utc>,
) -> rusqlite::Result<()> {
    let stamp = now.to_rfc3339();
    for j in judged {
        let old: Option<i64> = conn
            .query_row(
                "SELECT level FROM news_events WHERE key = ?1",
                [&j.key],
                |r| r.get(0),
            )
            .ok();
        if old.is_some_and(|l| l >= j.level) {
            continue;
        }
        conn.execute(
            "INSERT OR REPLACE INTO news_events (key, level, summary, source, link, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![j.key, j.level, j.summary, j.source, j.link, stamp],
        )?;
    }
    for (_, it) in cands {
        conn.execute(
            "INSERT OR REPLACE INTO news_seen (id, at) VALUES (?1, ?2)",
            rusqlite::params![item_key(it), stamp],
        )?;
    }
    let old = (now - Duration::days(3)).to_rfc3339();
    conn.execute("DELETE FROM news_seen WHERE at < ?1", [&old])?;
    conn.execute("DELETE FROM news_events WHERE at < ?1", [&old])?;
    Ok(())
}

/// Stored events still worth sending, as alerts.
pub fn pending_alerts(
    conn: &rusqlite::Connection,
    now: DateTime<Utc>,
) -> rusqlite::Result<Vec<Alert>> {
    let since = (now - Duration::hours(EVENT_TTL_HOURS)).to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT key, level, summary, source, link FROM news_events WHERE at >= ?1 ORDER BY at",
    )?;
    let rows = stmt.query_map([since], |r| {
        let key: String = r.get(0)?;
        let level: i64 = r.get(1)?;
        let summary: String = r.get(2)?;
        let source: String = r.get(3)?;
        let link: String = r.get(4)?;
        Ok(event_alert(key, level, &summary, &source, &link))
    })?;
    rows.collect()
}

pub fn event_alert(key: String, level: i64, summary: &str, source: &str, link: &str) -> Alert {
    let (priority, icon) = if level >= 2 {
        (Priority::P1, "🚨")
    } else {
        (Priority::P2, "📰")
    };
    Alert {
        priority,
        key,
        level,
        text: format!("{icon} {summary}（[{source}]({link})）"),
    }
}

/// Fetches the feeds, filters in code, asks the model only if anything is
/// left. Returns `false` when the model was needed but gave no usable answer
/// (the poll is then retried; nothing is marked seen).
pub async fn poll(
    config: &crate::config::Config,
    local: &LocalConfig,
    client: &reqwest::Client,
    now: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut items = Vec::new();
    for (feed, url) in FEEDS {
        match news_pipeline::get_text(client, url).await {
            Ok(body) => items.extend(
                news_pipeline::parse_feed(&body, feed)
                    .into_iter()
                    .map(|it| (*feed, it)),
            ),
            Err(e) => tracing::warn!(feed, "breaking-news feed failed: {e:#}"),
        }
    }
    let nearby = &local.alerts.nearby_suburbs;
    let (cands, known) = {
        let conn = super::open_state(&config.workspace_dir)?;
        let cands = candidates(items, &|id| is_seen(&conn, id), nearby, now);
        (cands, recent_events(&conn, now)?)
    };
    if cands.is_empty() {
        return Ok(true);
    }
    let request = build_request(&cands, &known, nearby, now, local.tz());
    let answer = match news_pipeline::ask_model(config, SYSTEM_PROMPT, &request).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                candidates = cands.len(),
                "breaking-news model call failed: {e:#}"
            );
            return Ok(false);
        }
    };
    let Some(judged) = parse_answer(&answer, &cands, &known) else {
        let head: String = answer.chars().take(200).collect();
        tracing::warn!(answer = %head, "breaking-news answer unreadable");
        return Ok(false);
    };
    tracing::info!(
        candidates = cands.len(),
        events = judged.len(),
        "breaking-news candidates judged"
    );
    let conn = super::open_state(&config.workspace_dir)?;
    record(&conn, &judged, &cands, now)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, link: &str, mins_ago: i64, now: DateTime<Utc>) -> Item {
        Item {
            source: "test".into(),
            title: title.into(),
            link: link.into(),
            published: Some(now - Duration::minutes(mins_ago)),
            text: String::new(),
            official: false,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-09-27T06:00:00Z".parse().unwrap()
    }

    fn mem() -> rusqlite::Connection {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        ensure_tables(&c).unwrap();
        c.execute_batch("CREATE TABLE notified (key TEXT PRIMARY KEY, level INTEGER, at TEXT);")
            .unwrap();
        c
    }

    #[test]
    fn filter_keeps_recent_danger_headlines_with_a_place() {
        let n = now();
        let nearby = vec!["Testville".to_string()];
        let items = vec![
            (
                "7NEWS",
                item("Man stabbed at train station", "https://a/1", 30, n),
            ),
            (
                "7NEWS",
                item("Council approves new park", "https://a/2", 30, n),
            ),
            (
                "7NEWS",
                item("Shooting in the west", "https://a/3", 5 * 60, n),
            ),
            (
                "ABC",
                item("Explosion at factory in Perth", "https://b/1", 10, n),
            ),
            (
                "ABC",
                item("Explosion at Testville shops", "https://b/2", 10, n),
            ),
            (
                "SMH",
                item("Sydney siege: police surround house", "https://c/1", 20, n),
            ),
            (
                "SMH",
                item("Sydney siege: police surround house", "https://c/1", 20, n),
            ),
        ];
        let out = candidates(items, &|_| false, &nearby, n);
        let links: Vec<&str> = out.iter().map(|(_, i)| i.link.as_str()).collect();
        assert_eq!(links, ["https://b/2", "https://c/1", "https://a/1"]);

        let seen = candidates(
            vec![("7NEWS", item("Man stabbed", "https://a/1", 30, n))],
            &|id| id == "a/1",
            &nearby,
            n,
        );
        assert!(seen.is_empty(), "already judged");
    }

    #[test]
    fn answer_maps_items_levels_and_repeats() {
        let n = now();
        let cands = vec![
            (
                "7NEWS",
                item("Gunman at large in Testville", "https://a/1", 5, n),
            ),
            (
                "Google News",
                item("Testville shooting - SMH", "https://g/1", 6, n),
            ),
        ];
        let known = vec![KnownEvent {
            key: "news:a/0".into(),
            level: 1,
            summary: "Testville 枪击".into(),
        }];
        let answer = r#"```json
{"events":[
 {"items":[0,1],"level":"P1","same_as":"E1","zh":"Testville 持枪者仍在逃，警方封锁周边"},
 {"items":[1],"level":"P2","same_as":null,"zh":"另一事件"},
 {"items":[9],"level":"P1","same_as":null,"zh":"越界编号"},
 {"items":[0],"level":"skip","same_as":null,"zh":"跳过"}
]}
```"#;
        let j = parse_answer(answer, &cands, &known).unwrap();
        assert_eq!(j.len(), 2);
        assert_eq!(j[0].key, "news:a/0", "same_as keeps the known key");
        assert_eq!(j[0].level, 2);
        assert_eq!(j[0].link, "https://a/1");
        assert_eq!(j[1].key, "news:g/1");
        assert_eq!(j[1].level, 1);

        assert_eq!(
            parse_answer(r#"{"events":[]}"#, &cands, &known),
            Some(vec![])
        );
        assert_eq!(parse_answer("服务繁忙", &cands, &known), None);
    }

    #[test]
    fn record_upgrades_only_when_danger_rises() {
        let n = now();
        let c = mem();
        let cand = vec![("7NEWS", item("Stabbing", "https://a/1", 5, n))];
        let j = |level, summary: &str| Judged {
            key: "news:a/1".into(),
            level,
            summary: summary.into(),
            source: "7NEWS".into(),
            link: "https://a/1".into(),
        };
        record(&c, &[j(1, "第一次")], &cand, n).unwrap();
        assert!(is_seen(&c, "a/1"));
        record(&c, &[j(1, "重复报道")], &[], n).unwrap();
        let a = pending_alerts(&c, n).unwrap();
        assert_eq!(a.len(), 1);
        assert!(a[0].text.contains("第一次"));
        assert_eq!(a[0].priority, Priority::P2);

        record(&c, &[j(2, "升级为正在进行的袭击")], &[], n).unwrap();
        let a = pending_alerts(&c, n).unwrap();
        assert_eq!(a[0].level, 2);
        assert_eq!(a[0].priority, Priority::P1);
        assert!(a[0]
            .text
            .starts_with("🚨 升级为正在进行的袭击（[7NEWS](https://a/1)）"));

        assert!(pending_alerts(&c, n + Duration::hours(13))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn request_lists_known_events_and_candidates() {
        let n = now();
        let cands = vec![(
            "7NEWS",
            item("Man stabbed in Testville", "https://a/1", 5, n),
        )];
        let known = vec![KnownEvent {
            key: "k".into(),
            level: 2,
            summary: "某地围困".into(),
        }];
        let r = build_request(
            &cands,
            &known,
            &["Testville".into()],
            n,
            chrono_tz::Australia::Sydney,
        );
        assert!(r.contains("现在是悉尼时间 2026-09-27 16:00"));
        assert!(r.contains("家附近的区：Testville"));
        assert!(r.contains("E1 [P1] 某地围困"));
        assert!(r.contains("0. [7NEWS 15:55] Man stabbed in Testville"));
    }
}
