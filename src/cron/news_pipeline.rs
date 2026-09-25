//! elfClaw 2026-09-25: code-run daily news pushes (`JobType::News`).
//!
//! The old flow gave the news worker (an LLM agent) a list of URLs and let it
//! fetch them one tool call at a time: 8–15 model calls per push, frequent
//! runs into the 15-iteration limit, rewritten links, and a picture of the
//! world limited to whatever the model managed to fetch. Now code does every
//! deterministic step and the model is asked exactly once, to choose and
//! write:
//!
//! 1. fetch every usable source of the slot concurrently — RSS/Atom feeds,
//!    public Telegram channels (`t.me/s/<name>`), Polymarket events with big
//!    24 h odds moves, Hacker News, and plain web pages via cf-crawler;
//! 2. per-source keyword filter, freshness window, per-source cap;
//! 3. de-duplicate across sources and against everything pushed in the last
//!    `HISTORY_DAYS` days (local SQLite `state/news_history.db`);
//! 4. market quotes (Yahoo Finance chart API + Bank of China USD rates) for
//!    slots with `quotes = true` — numbers come straight from the data, never
//!    from the model;
//! 5. one model call returns only `{id, category, title, summary}` per chosen
//!    item; the link of each item is taken from the fetched item by id, so
//!    the model cannot alter or invent links. If the model is unavailable the
//!    push still goes out with the newest items' original titles;
//! 6. per-source fetch results feed the existing failure/ban counters.

use crate::config::Config;
use crate::cron::news::{self, NewsRules, Slot, SlotKind, Source, SourceResult};
use crate::providers::{self, Provider, ProviderRuntimeOptions};
use crate::security::SecurityPolicy;
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

/// Items per push when the slot does not set `max_items`.
pub const DEFAULT_MAX_ITEMS: usize = 20;
/// Newest items kept per source before de-duplication.
const PER_SOURCE_CAP: usize = 12;
/// Candidates handed to the model at most (keeps the prompt bounded).
const MAX_CANDIDATES: usize = 160;
/// Dated items older than this are dropped.
const FRESH_HOURS: i64 = 36;
/// An item pushed within this many days is never pushed again.
const HISTORY_DAYS: i64 = 14;
const FETCH_TIMEOUT_SECS: u64 = 25;
const FETCH_CONCURRENCY: usize = 6;
/// Polymarket: minimum absolute 24 h probability move / 24 h volume (USD).
const POLYMARKET_MIN_MOVE: f64 = 0.08;
const POLYMARKET_MIN_VOLUME: f64 = 20_000.0;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/128.0 Safari/537.36";
/// Chinese state / party media: kept only as background, flagged as such.
const OFFICIAL_MEDIA_HOSTS: &[&str] = &[
    "xinhuanet.com",
    "news.cn",
    "people.com.cn",
    "globaltimes.cn",
    "chinadaily.com.cn",
    "cctv.com",
    "cgtn.com",
    "gov.cn",
    "chinanews.com.cn",
    "ecns.cn",
    "qstheory.cn",
];

// ── items ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    /// Display name of the source.
    pub source: String,
    pub title: String,
    pub link: String,
    pub published: Option<DateTime<Utc>>,
    /// Short plain-text excerpt.
    pub text: String,
    /// From Chinese state / party media (see `OFFICIAL_MEDIA_HOSTS`).
    pub official: bool,
}

pub(super) fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_string())
        })
        .unwrap_or_default()
}

fn is_official(url: &str) -> bool {
    let host = host_of(url);
    OFFICIAL_MEDIA_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Identity of a link for de-duplication: no scheme, no `www.`, no fragment,
/// no tracking parameters, no trailing slash.
pub fn url_key(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url.trim()) else {
        return url.trim().to_lowercase();
    };
    parsed.set_fragment(None);
    let kept: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !(k.starts_with("utm_")
                || matches!(k.as_str(), "fbclid" | "gclid" | "ref" | "ref_src" | "cmpid"))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        parsed.set_query(None);
    } else {
        parsed.query_pairs_mut().clear().extend_pairs(kept);
    }
    let host = parsed
        .host_str()
        .unwrap_or_default()
        .trim_start_matches("www.")
        .to_lowercase();
    let path = parsed.path().trim_end_matches('/');
    match parsed.query() {
        Some(q) => format!("{host}{path}?{q}"),
        None => format!("{host}{path}"),
    }
}

/// Identity of a headline: letters/digits/CJK only, lower-case, first 60.
pub fn title_key(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .take(60)
        .collect()
}

/// Decode the HTML/XML entities that appear in feeds and Telegram pages.
pub(super) fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // Entities are short ASCII; look at most 12 bytes ahead, on a char
        // boundary (the text after '&' may be CJK).
        let window = &tail[..crate::util::floor_utf8_char_boundary(tail, tail.len().min(12))];
        let Some(semi) = window.find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tags → spaces, entities decoded, whitespace collapsed, cut to `max` chars.
pub(super) fn plain_text(html: &str, max: usize) -> String {
    let no_tags = regex_replace_tags(html);
    let decoded = decode_entities(&no_tags);
    let collapsed = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::util::truncate_with_ellipsis(&collapsed, max)
}

fn regex_replace_tags(html: &str) -> String {
    static TAGS: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?is)<br\s*/?>|</p>|<[^>]+>").expect("valid regex")
    });
    TAGS.replace_all(html, " ").into_owned()
}

fn parse_date(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| DateTime::parse_from_rfc2822(s))
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            // RFC 2822 with a named zone chrono does not know ("EST", "AEST"…).
            let cut = s.rsplit_once(' ').map_or(s, |(head, _)| head);
            chrono::NaiveDateTime::parse_from_str(cut, "%a, %d %b %Y %H:%M:%S")
                .ok()
                .map(|n| Utc.from_utc_datetime(&n))
        })
}

// ── source parsers ───────────────────────────────────────────────────────

/// RSS 2.0, RSS 1.0 (RDF) and Atom. Malformed feeds yield what was read.
pub fn parse_feed(xml: &str, source: &str) -> Vec<Item> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    #[derive(Default)]
    struct Draft {
        title: String,
        link: String,
        date: String,
        text: String,
        publisher: String,
    }

    let mut reader = Reader::from_str(xml);
    let mut items = Vec::new();
    let mut draft: Option<Draft> = None;
    let mut field: Option<String> = None;
    loop {
        let event = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(event) => event,
        };
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_lowercase();
                if name == "item" || name == "entry" {
                    draft = Some(Draft::default());
                    field = None;
                    continue;
                }
                let Some(d) = draft.as_mut() else { continue };
                if name == "link" {
                    // Atom: <link href="…" rel="alternate"/>
                    let mut href = None;
                    let mut rel = None;
                    for attr in e.attributes().flatten() {
                        let value = attr
                            .unescape_value()
                            .map(|v| v.into_owned())
                            .unwrap_or_default();
                        match attr.key.local_name().as_ref() {
                            b"href" => href = Some(value),
                            b"rel" => rel = Some(value),
                            _ => {}
                        }
                    }
                    if let Some(href) = href {
                        if d.link.is_empty() && rel.as_deref().is_none_or(|r| r == "alternate") {
                            d.link = href;
                        }
                        continue;
                    }
                }
                field = Some(name);
            }
            Event::Text(t) => {
                if let (Some(d), Some(f)) = (draft.as_mut(), field.as_deref()) {
                    let text = t
                        .unescape()
                        .map(|v| v.into_owned())
                        .unwrap_or_else(|_| decode_entities(&String::from_utf8_lossy(t.as_ref())));
                    append_field(d, f, &text);
                }
            }
            Event::CData(c) => {
                if let (Some(d), Some(f)) = (draft.as_mut(), field.as_deref()) {
                    append_field(d, f, &String::from_utf8_lossy(&c.into_inner()));
                }
            }
            Event::End(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_lowercase();
                if name == "item" || name == "entry" {
                    if let Some(d) = draft.take() {
                        let title = plain_text(&d.title, 200);
                        let link = d.link.trim().to_string();
                        if !title.is_empty() && link.starts_with("http") {
                            let name = if d.publisher.trim().is_empty() {
                                source.to_string()
                            } else {
                                format!("{source}·{}", d.publisher.trim())
                            };
                            items.push(Item {
                                official: is_official(&link),
                                source: name,
                                title,
                                published: parse_date(&d.date),
                                text: plain_text(&d.text, 300),
                                link,
                            });
                        }
                    }
                }
                field = None;
            }
            _ => {}
        }
    }

    fn append_field(d: &mut Draft, field: &str, text: &str) {
        match field {
            "title" => d.title.push_str(text),
            "link" => d.link.push_str(text.trim()),
            "pubdate" | "published" | "updated" | "date" => {
                if d.date.is_empty() {
                    d.date = text.to_string();
                }
            }
            "description" | "summary" | "content" | "encoded" => {
                if d.text.len() < 2000 {
                    d.text.push_str(text);
                }
            }
            "source" => d.publisher.push_str(text),
            _ => {}
        }
    }
    items
}

/// Public Telegram channel web preview (`https://t.me/s/<channel>`).
pub fn parse_telegram(html: &str, channel: &str, source: &str) -> Vec<Item> {
    static POST: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"data-post="([^"/]+/\d+)""#).expect("valid regex")
    });
    static TEXT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?s)class="tgme_widget_message_text[^"]*"[^>]*>(.*?)</div>"#)
            .expect("valid regex")
    });
    static TIME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"<time[^>]+datetime="([^"]+)""#).expect("valid regex")
    });
    let _ = channel;
    let starts: Vec<(usize, &str)> = POST
        .captures_iter(html)
        .filter_map(|c| {
            c.get(1)
                .map(|m| (c.get(0).map_or(0, |m0| m0.start()), m.as_str()))
        })
        .collect();
    let mut items = Vec::new();
    for (i, (start, post)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(html.len(), |(s, _)| *s);
        let block = &html[*start..end];
        let Some(raw) = TEXT.captures(block).and_then(|c| c.get(1)) else {
            continue;
        };
        let text = plain_text(raw.as_str(), 600);
        if text.chars().count() < 12 {
            continue;
        }
        let title: String = text.chars().take(140).collect();
        items.push(Item {
            source: source.to_string(),
            title,
            link: format!("https://t.me/{post}"),
            published: TIME
                .captures(block)
                .and_then(|c| c.get(1))
                .and_then(|m| parse_date(m.as_str())),
            text,
            official: false,
        });
    }
    items
}

/// Polymarket gamma `/events` JSON → markets whose odds moved a lot in 24 h.
pub fn parse_polymarket(json: &Value, source: &str, now: DateTime<Utc>) -> Vec<Item> {
    let mut items = Vec::new();
    for event in json.as_array().into_iter().flatten() {
        let slug = event
            .get("slug")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for market in event
            .get("markets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if market.get("closed").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            let num = |k: &str| {
                market.get(k).and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
            };
            let (Some(change), Some(volume)) = (num("oneDayPriceChange"), num("volume24hr")) else {
                continue;
            };
            if change.abs() < POLYMARKET_MIN_MOVE || volume < POLYMARKET_MIN_VOLUME {
                continue;
            }
            let question = market
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let price = num("lastTradePrice").or_else(|| {
                market
                    .get("outcomePrices")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
                    .and_then(|v| v.first().and_then(|p| p.parse().ok()))
            });
            let odds = price.map_or(String::new(), |p| format!("当前概率 {:.0}%，", p * 100.0));
            items.push(Item {
                source: source.to_string(),
                title: format!("[预测市场] {question}"),
                link: format!("https://polymarket.com/event/{slug}"),
                published: Some(now),
                text: format!(
                    "{odds}24小时变化 {:+.0} 个百分点，24小时成交 ${volume:.0}",
                    change * 100.0
                ),
                official: false,
            });
        }
    }
    items
}

/// Hacker News via Algolia (`/api/v1/search?tags=front_page`).
pub fn parse_hn(json: &Value, source: &str) -> Vec<Item> {
    json.get("hits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hit| {
            let title = hit.get("title").and_then(Value::as_str)?.to_string();
            let id = hit
                .get("objectID")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let link = hit
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("http"))
                .map_or_else(
                    || format!("https://news.ycombinator.com/item?id={id}"),
                    str::to_string,
                );
            let points = hit.get("points").and_then(Value::as_i64).unwrap_or(0);
            Some(Item {
                source: source.to_string(),
                official: is_official(&link),
                title,
                link,
                published: hit
                    .get("created_at_i")
                    .and_then(Value::as_i64)
                    .and_then(|t| Utc.timestamp_opt(t, 0).single()),
                text: format!("HN {points} 分"),
            })
        })
        .collect()
}

/// cf-crawler listing result (`items: [{title, url}]`) → undated items.
fn parse_listing(v: &Value, source: &str) -> Vec<Item> {
    v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|it| {
            let title = plain_text(it.get("title").and_then(Value::as_str)?, 200);
            let link = it.get("url").and_then(Value::as_str)?.to_string();
            (title.chars().count() >= 12 && link.starts_with("http")).then(|| Item {
                source: source.to_string(),
                official: is_official(&link),
                title,
                link,
                published: None,
                text: String::new(),
            })
        })
        .collect()
}

// ── fetching ─────────────────────────────────────────────────────────────

enum Kind {
    Telegram(String),
    Polymarket,
    HackerNews,
    /// Feed if the body is a feed, otherwise a web page for cf-crawler.
    Auto,
}

fn kind_of(url: &str) -> Kind {
    let host = host_of(url);
    if host == "t.me" {
        let channel = url
            .split("t.me/")
            .nth(1)
            .unwrap_or_default()
            .trim_start_matches("s/")
            .split(['/', '?'])
            .next()
            .unwrap_or_default()
            .to_string();
        return Kind::Telegram(channel);
    }
    if host == "gamma-api.polymarket.com" {
        return Kind::Polymarket;
    }
    if host == "hn.algolia.com" {
        return Kind::HackerNews;
    }
    Kind::Auto
}

pub(super) fn display_name(src: &Source) -> String {
    if !src.name.trim().is_empty() {
        return src.name.trim().to_string();
    }
    match kind_of(&src.url) {
        Kind::Telegram(channel) => format!("@{channel}"),
        _ => host_of(&src.url),
    }
}

pub(super) fn looks_like_feed(body: &str) -> bool {
    let head: String = body.chars().take(600).collect::<String>().to_lowercase();
    head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:rdf")
}

pub(super) async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("HTTP {}", status.as_u16());
    }
    Ok(body)
}

async fn fetch_source(
    client: &reqwest::Client,
    security: &SecurityPolicy,
    src: &Source,
    now: DateTime<Utc>,
) -> Result<Vec<Item>> {
    let name = display_name(src);
    match kind_of(&src.url) {
        Kind::Telegram(channel) => {
            let html = get_text(client, &format!("https://t.me/s/{channel}")).await?;
            if !html.contains("tgme_widget_message") {
                anyhow::bail!("频道没有公开预览（可能不存在或是私有频道）");
            }
            Ok(parse_telegram(&html, &channel, &name))
        }
        Kind::Polymarket => {
            let json: Value = serde_json::from_str(&get_text(client, &src.url).await?)?;
            Ok(parse_polymarket(&json, &name, now))
        }
        Kind::HackerNews => {
            let json: Value = serde_json::from_str(&get_text(client, &src.url).await?)?;
            Ok(parse_hn(&json, &name))
        }
        Kind::Auto => {
            let body = get_text(client, &src.url).await;
            match body {
                Ok(body) if looks_like_feed(&body) => Ok(parse_feed(&body, &name)),
                // Not a feed (or plain HTTP was refused): let cf-crawler
                // extract the article list, with its anti-bot fallbacks.
                _ => {
                    let listing =
                        crate::tools::cf_crawler::scrape_listing(security, &src.url).await?;
                    Ok(parse_listing(&listing, &name))
                }
            }
        }
    }
}

/// Keyword filter, freshness window, newest-first cap.
fn select_from_source(mut items: Vec<Item>, src: &Source, now: DateTime<Utc>) -> Vec<Item> {
    let words: Vec<String> = src.filter.iter().map(|w| w.to_lowercase()).collect();
    items.retain(|it| {
        let fresh = it
            .published
            .is_none_or(|p| now.signed_duration_since(p) <= chrono::Duration::hours(FRESH_HOURS));
        let wanted = words.is_empty() || {
            let hay = format!("{} {}", it.title, it.text).to_lowercase();
            words.iter().any(|w| hay.contains(w.as_str()))
        };
        fresh && wanted
    });
    items.sort_by(|a, b| b.published.cmp(&a.published));
    items.truncate(PER_SOURCE_CAP);
    items
}

// ── history ──────────────────────────────────────────────────────────────

fn history_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join("state").join("news_history.db")
}

fn open_history(workspace: &Path) -> Result<rusqlite::Connection> {
    let path = history_path(workspace);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pushed (
             url_key TEXT PRIMARY KEY,
             title_key TEXT NOT NULL,
             slot TEXT NOT NULL,
             pushed_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS pushed_title ON pushed(title_key);",
    )?;
    Ok(conn)
}

/// (url keys, title keys) pushed since `since`.
fn pushed_since(
    conn: &rusqlite::Connection,
    since: DateTime<Utc>,
) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut stmt = conn.prepare("SELECT url_key, title_key FROM pushed WHERE pushed_at >= ?1")?;
    let rows = stmt.query_map([since.to_rfc3339()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut urls = HashSet::new();
    let mut titles = HashSet::new();
    for row in rows {
        let (u, t) = row?;
        urls.insert(u);
        if !t.is_empty() {
            titles.insert(t);
        }
    }
    Ok((urls, titles))
}

fn record_pushed(
    conn: &rusqlite::Connection,
    items: &[&Item],
    slot: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    for it in items {
        conn.execute(
            "INSERT OR REPLACE INTO pushed (url_key, title_key, slot, pushed_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![url_key(&it.link), title_key(&it.title), slot, now.to_rfc3339()],
        )?;
    }
    conn.execute(
        "DELETE FROM pushed WHERE pushed_at < ?1",
        [(now - chrono::Duration::days(HISTORY_DAYS * 2)).to_rfc3339()],
    )?;
    Ok(())
}

/// Drop items already pushed and duplicates within this batch (same link or
/// same headline from another source). Order is preserved.
pub fn dedupe(
    items: Vec<Item>,
    pushed_urls: &HashSet<String>,
    pushed_titles: &HashSet<String>,
) -> Vec<Item> {
    let mut urls = HashSet::new();
    let mut titles = HashSet::new();
    items
        .into_iter()
        .filter(|it| {
            let u = url_key(&it.link);
            let t = title_key(&it.title);
            let dup = pushed_urls.contains(&u)
                || (!t.is_empty() && pushed_titles.contains(&t))
                || !urls.insert(u)
                || (!t.is_empty() && !titles.insert(t));
            !dup
        })
        .collect()
}

// ── market quotes ────────────────────────────────────────────────────────

/// (Yahoo symbol, label, decimals)
const QUOTES: &[(&str, &str, usize)] = &[
    ("CL=F", "WTI原油", 2),
    ("BZ=F", "布伦特", 2),
    ("GC=F", "黄金", 1),
    ("SI=F", "白银", 2),
    ("^GSPC", "标普500", 0),
    ("^IXIC", "纳指", 0),
    ("^DJI", "道指", 0),
    ("^AXJO", "澳股200", 0),
    ("EURUSD=X", "欧元/美元", 4),
    ("AUDUSD=X", "澳元/美元", 4),
    ("CNY=X", "美元/人民币", 4),
    ("BTC-USD", "比特币", 0),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub label: &'static str,
    pub decimals: usize,
    pub price: f64,
    /// Change vs. the previous daily close, in percent.
    pub change_pct: Option<f64>,
}

/// Bank of China USD rates per 100 USD: (现汇买入, 现汇卖出, 中行折算价, 发布时间).
#[derive(Debug, Clone, PartialEq)]
pub struct BocUsd {
    pub buy: String,
    pub sell: String,
    pub middle: String,
    pub time: String,
}

pub fn parse_yahoo(json: &Value) -> Option<(f64, Option<f64>)> {
    let result = json.pointer("/chart/result/0")?;
    let price = result.pointer("/meta/regularMarketPrice")?.as_f64()?;
    let closes: Vec<f64> = result
        .pointer("/indicators/quote/0/close")
        .and_then(Value::as_array)
        .map(|v| v.iter().filter_map(Value::as_f64).collect())
        .unwrap_or_default();
    let previous = if closes.len() >= 2 {
        Some(closes[closes.len() - 2])
    } else {
        result
            .pointer("/meta/chartPreviousClose")
            .and_then(Value::as_f64)
    };
    let change = previous
        .filter(|p| *p > 0.0)
        .map(|p| (price / p - 1.0) * 100.0);
    Some((price, change))
}

pub fn parse_boc(html: &str) -> Option<BocUsd> {
    static ROW: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?s)<td>美元</td>(.*?)</tr>").expect("valid regex")
    });
    static CELL: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"<td[^>]*>([^<]*)</td>").expect("valid regex")
    });
    let row = ROW.captures(html)?.get(1)?.as_str();
    let cells: Vec<String> = CELL
        .captures_iter(row)
        .map(|c| c[1].trim().to_string())
        .collect();
    // 现汇买入, 现钞买入, 现汇卖出, 现钞卖出, 中行折算价, 发布日期, 发布时间
    (cells.len() >= 7).then(|| BocUsd {
        buy: cells[0].clone(),
        sell: cells[2].clone(),
        middle: cells[4].clone(),
        time: cells[6].clone(),
    })
}

async fn fetch_quote(
    client: reqwest::Client,
    symbol: &'static str,
    label: &'static str,
    decimals: usize,
) -> Option<Quote> {
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?range=5d&interval=1d",
        urlencoding::encode(symbol)
    );
    let json: Value = serde_json::from_str(&get_text(&client, &url).await.ok()?).ok()?;
    let (price, change_pct) = parse_yahoo(&json)?;
    Some(Quote {
        label,
        decimals,
        price,
        change_pct,
    })
}

async fn fetch_quotes(client: reqwest::Client) -> (Vec<Quote>, Option<BocUsd>) {
    // Owned values in the futures (no borrowing closures): the scheduler task
    // must stay `Send` for every lifetime.
    let tasks: Vec<_> = QUOTES
        .iter()
        .map(|&(symbol, label, decimals)| fetch_quote(client.clone(), symbol, label, decimals))
        .collect();
    let yahoo = stream::iter(tasks).buffered(4).collect::<Vec<_>>();
    let boc_client = client.clone();
    let boc = async move {
        parse_boc(
            &get_text(&boc_client, "https://www.boc.cn/sourcedb/whpj/")
                .await
                .ok()?,
        )
    };
    let (quotes, boc) = tokio::join!(yahoo, boc);
    (quotes.into_iter().flatten().collect(), boc)
}

fn fmt_number(v: f64, decimals: usize) -> String {
    let raw = format!("{v:.decimals$}");
    let (int, frac) = raw
        .split_once('.')
        .map_or((raw.as_str(), None), |(i, f)| (i, Some(f)));
    let (sign, digits) = int.strip_prefix('-').map_or(("", int), |d| ("-", d));
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

pub fn render_quotes(quotes: &[Quote], boc: Option<&BocUsd>) -> String {
    if quotes.is_empty() && boc.is_none() {
        return "📈 行情：暂时取不到数据\n".to_string();
    }
    let by_label: HashMap<&str, &Quote> = quotes.iter().map(|q| (q.label, q)).collect();
    let cell = |label: &str| {
        by_label.get(label).map(|q| {
            let change = q
                .change_pct
                .map_or(String::new(), |c| format!("（{c:+.2}%）"));
            format!("{} {}{change}", q.label, fmt_number(q.price, q.decimals))
        })
    };
    let line = |icon: &str, labels: &[&str]| {
        let cells: Vec<String> = labels.iter().filter_map(|l| cell(l)).collect();
        (!cells.is_empty()).then(|| format!("{icon} {}", cells.join(" ｜ ")))
    };
    let mut out = String::from("📈 **行情**\n");
    for l in [
        line("🛢", &["WTI原油", "布伦特"]),
        line("🥇", &["黄金", "白银"]),
        line("🇺🇸", &["标普500", "纳指", "道指"]),
        line("🇦🇺", &["澳股200"]),
        line("💱", &["欧元/美元", "澳元/美元", "美元/人民币"]),
        line("₿", &["比特币"]),
    ]
    .into_iter()
    .flatten()
    {
        out.push_str(&l);
        out.push('\n');
    }
    if let Some(b) = boc {
        let _ = writeln!(
            out,
            "🏦 中行美元牌价（每100美元）：现汇买入 {} ｜ 现汇卖出 {} ｜ 折算价 {}（{}）",
            b.buy, b.sell, b.middle, b.time
        );
    }
    out
}

// ── selection by the model ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Pick {
    id: usize,
    #[serde(default)]
    category: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
}

#[derive(Debug, Deserialize)]
struct Picks {
    items: Vec<Pick>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chosen {
    pub index: usize,
    pub category: String,
    pub title: String,
    pub summary: String,
}

const SELECT_SYSTEM: &str = "你是一名资深新闻编辑，为一位住在悉尼的华人读者挑选并改写新闻。\n\
规则：\n\
1. 只能从给出的候选条目里挑选，用条目编号 id 引用；不要编造任何条目、数字或事实，不要写链接。\n\
2. 优先：重大突发、对市场/政策/安全有实际影响的消息、战场与军事科技新动态、真正的新进展；\
跳过：重复报道、软文广告、体育娱乐八卦、与本时段重点无关的内容。加密货币和区块链新闻一律不要。\n\
3. 标注「官方口径」的条目来自中共党政媒体：它们是宣传，不是新闻。只有当它透露了政策动向时才可以选，\
并在摘要里写明这是官方说法；不要把宣传内容当成事实陈述。\n\
4. 标题和摘要用简体中文，标题不超过 30 字，摘要一句话、不超过 60 字；专有名词可保留英文。\n\
5. category 用简短的中文分类名（例如：国际、两岸三地、美国、澳洲、俄乌战场、军事科技、AI、金融支付、\
无线电、科技），同类条目用同一个分类名。\n\
6. 只输出 JSON，格式：{\"items\":[{\"id\":编号,\"category\":\"分类\",\"title\":\"中文标题\",\"summary\":\"一句话摘要\"}]}，\
不要输出任何其他文字。";

fn relative_age(published: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match published {
        None => "时间未知".to_string(),
        Some(p) => {
            let minutes = now.signed_duration_since(p).num_minutes().max(0);
            if minutes < 60 {
                format!("{minutes}分钟前")
            } else {
                format!("{}小时前", minutes / 60)
            }
        }
    }
}

fn build_request(slot: &Slot, candidates: &[Item], max_items: usize, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "时段：{}\n本时段重点：{}\n请从下面 {} 条候选里挑出最值得推送的 {} 条左右（不够就少选，不要凑数）。\n\n候选：\n",
        slot.name,
        if slot.focus.trim().is_empty() { "（无）" } else { slot.focus.trim() },
        candidates.len(),
        max_items
    );
    for (i, it) in candidates.iter().enumerate() {
        let official = if it.official {
            "【官方口径】"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "[{i}] {}{official} | {} | {} | {}",
            it.source,
            relative_age(it.published, now),
            it.title,
            crate::util::truncate_with_ellipsis(&it.text, 220)
        );
    }
    out
}

/// Parse the model's JSON answer; tolerant of code fences and prose around it.
pub fn parse_picks(answer: &str, candidates: usize, max_items: usize) -> Option<Vec<Chosen>> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let picks: Picks = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let mut seen = HashSet::new();
    let chosen: Vec<Chosen> = picks
        .items
        .into_iter()
        .filter(|p| p.id < candidates && seen.insert(p.id))
        .map(|p| Chosen {
            index: p.id,
            category: p.category.trim().to_string(),
            title: p.title.trim().to_string(),
            summary: p.summary.trim().to_string(),
        })
        .take(max_items)
        .collect();
    (!chosen.is_empty()).then_some(chosen)
}

/// Used when the model is unavailable: newest items, round-robin across
/// sources so one busy feed cannot fill the push, original titles.
pub fn fallback_picks(candidates: &[Item], max_items: usize) -> Vec<Chosen> {
    let mut by_source: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, it) in candidates.iter().enumerate() {
        if it.official {
            continue;
        }
        match by_source.iter_mut().find(|(s, _)| *s == it.source) {
            Some((_, v)) => v.push(i),
            None => by_source.push((it.source.clone(), vec![i])),
        }
    }
    let mut chosen = Vec::new();
    let mut round = 0;
    while chosen.len() < max_items {
        let mut added = false;
        for (source, list) in &by_source {
            if let Some(&i) = list.get(round) {
                chosen.push(Chosen {
                    index: i,
                    category: source.clone(),
                    title: candidates[i].title.clone(),
                    summary: String::new(),
                });
                added = true;
                if chosen.len() == max_items {
                    break;
                }
            }
        }
        if !added {
            break;
        }
        round += 1;
    }
    chosen
}

pub(super) async fn ask_model(config: &Config, system: &str, request: &str) -> Result<String> {
    let provider_name = config.default_provider.as_deref().unwrap_or("gemini");
    let options = ProviderRuntimeOptions {
        zeroclaw_dir: config.config_path.parent().map(std::path::PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        reasoning_level: config.provider.reasoning_level,
        ..ProviderRuntimeOptions::default()
    };
    let provider: Box<dyn Provider> = providers::create_resilient_provider_with_options(
        provider_name,
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &options,
    )?;
    let model = config
        .worker_model
        .as_deref()
        .or(config.summary_model.as_deref())
        .or(config.default_model.as_deref())
        .unwrap_or("gemini-3.5-flash");
    provider
        .chat_with_system(Some(system), request, model, 0.3)
        .await
}

// ── rendering ────────────────────────────────────────────────────────────

fn escape_link_text(s: &str) -> String {
    s.replace('[', "(").replace(']', ")")
}

pub fn render(
    slot: &Slot,
    local_time: &str,
    quotes: Option<&str>,
    candidates: &[Item],
    chosen: &[Chosen],
    model_ok: bool,
    footer: &str,
) -> String {
    let mut out = format!("📰 **{}** | {local_time}\n", slot.name);
    if !model_ok {
        out.push_str("⚠️ 模型暂时不可用，以下为按时间挑选的原文标题\n");
    }
    if let Some(q) = quotes {
        out.push('\n');
        out.push_str(q);
    }
    let mut groups: Vec<(&str, Vec<&Chosen>)> = Vec::new();
    for c in chosen {
        match groups.iter_mut().find(|(k, _)| *k == c.category.as_str()) {
            Some((_, v)) => v.push(c),
            None => groups.push((c.category.as_str(), vec![c])),
        }
    }
    for (category, list) in groups {
        let heading = if category.is_empty() {
            "其他"
        } else {
            category
        };
        let _ = write!(out, "\n**【{heading}】**\n");
        for c in list {
            let item = &candidates[c.index];
            let title = if c.title.is_empty() {
                &item.title
            } else {
                &c.title
            };
            let official = if item.official {
                "🏛官方口径 "
            } else {
                ""
            };
            let summary = if c.summary.is_empty() {
                String::new()
            } else {
                format!(" — {}", c.summary)
            };
            let _ = writeln!(
                out,
                "• {official}[{}]({}){summary}（{}）",
                escape_link_text(title),
                item.link,
                item.source
            );
        }
    }
    if chosen.is_empty() {
        out.push_str("\n本时段没有新的内容（候选都已推送过或不符合时效）。\n");
    }
    if !footer.is_empty() {
        out.push('\n');
        out.push_str(footer);
    }
    out
}

// ── the job ──────────────────────────────────────────────────────────────

pub(super) fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()?)
}

pub(super) fn local_time_label(rules: &NewsRules, now: DateTime<Utc>) -> String {
    const WEEKDAYS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    match rules.tz.parse::<chrono_tz::Tz>() {
        Ok(tz) => {
            let local = now.with_timezone(&tz);
            let weekday =
                WEEKDAYS[chrono::Datelike::weekday(&local).num_days_from_monday() as usize];
            format!(
                "{} {weekday} {}",
                local.format("%m-%d"),
                local.format("%H:%M")
            )
        }
        Err(_) => now.format("%m-%d %H:%M UTC").to_string(),
    }
}

/// Run one slot push. Returns the message to deliver.
pub async fn run_slot(config: &Config, slot_name: &str) -> Result<String> {
    let rules = news::load_rules(config)?;
    let data = news::load_data(config)?;
    let slot = data
        .slots
        .iter()
        .find(|s| s.name == slot_name)
        .cloned()
        .with_context(|| format!("{} 里没有名为 '{slot_name}' 的时段", news::DATA_FILE))?;
    let sources: Vec<Source> = news::usable_sources(&slot, &data)
        .into_iter()
        .cloned()
        .collect();
    anyhow::ensure!(
        !sources.is_empty(),
        "时段 '{slot_name}' 没有可用的新闻源（都被封禁了）"
    );
    match slot.kind {
        SlotKind::News => run_news(config, &rules, &slot, sources, SELECT_SYSTEM).await,
        SlotKind::Expo => {
            Box::pin(crate::cron::expo_pipeline::run(
                config, &rules, &slot, sources,
            ))
            .await
        }
        SlotKind::Adult => {
            Box::pin(crate::cron::adult_pipeline::run(
                config, &rules, &slot, sources,
            ))
            .await
        }
    }
}

/// The headline push; `system` is the editor prompt (news or adult industry).
pub(super) async fn run_news(
    config: &Config,
    rules: &NewsRules,
    slot: &Slot,
    sources: Vec<Source>,
    system: &str,
) -> Result<String> {
    let slot_name = slot.name.as_str();
    let max_items = slot.max_items.unwrap_or(DEFAULT_MAX_ITEMS).max(1);
    let now = Utc::now();
    let client = http_client()?;
    let security = std::sync::Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    // 1–2. fetch + per-source selection, concurrently; quotes alongside.
    // Each future owns its inputs (see `fetch_quotes`).
    let tasks: Vec<_> = sources
        .into_iter()
        .map(|src| {
            let client = client.clone();
            let security = std::sync::Arc::clone(&security);
            async move {
                let result = fetch_source(&client, &security, &src, now).await;
                (src, result)
            }
        })
        .collect();
    let fetches = stream::iter(tasks)
        .buffered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>();
    let quote_client = client.clone();
    let wants_quotes = slot.quotes;
    let quotes = async move {
        if wants_quotes {
            let (q, boc) = fetch_quotes(quote_client).await;
            Some(render_quotes(&q, boc.as_ref()))
        } else {
            None
        }
    };
    let (fetched, quotes) = tokio::join!(fetches, quotes);

    let mut results = Vec::new();
    let mut failed = Vec::new();
    let mut per_source: Vec<Vec<Item>> = Vec::new();
    for (src, result) in fetched {
        match result {
            Ok(items) => {
                results.push(SourceResult {
                    url: src.url.clone(),
                    ok: true,
                    reason: String::new(),
                });
                per_source.push(select_from_source(items, &src, now));
            }
            Err(e) => {
                let reason = crate::util::truncate_with_ellipsis(&format!("{e:#}"), 120);
                tracing::warn!(slot = slot_name, source = %src.url, "news source failed: {reason}");
                failed.push(display_name(&src));
                results.push(SourceResult {
                    url: src.url.clone(),
                    ok: false,
                    reason,
                });
            }
        }
    }

    // 3. interleave sources (so the candidate cap is fair), then de-duplicate.
    let mut interleaved = Vec::new();
    let longest = per_source.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for list in &per_source {
            if let Some(it) = list.get(i) {
                interleaved.push(it.clone());
            }
        }
    }
    let history = open_history(&config.workspace_dir)?;
    let (pushed_urls, pushed_titles) =
        pushed_since(&history, now - chrono::Duration::days(HISTORY_DAYS))?;
    let mut candidates = dedupe(interleaved, &pushed_urls, &pushed_titles);
    candidates.truncate(MAX_CANDIDATES);

    // 5. one model call; deterministic fallback.
    let (chosen, model_ok) = if candidates.is_empty() {
        (Vec::new(), true)
    } else {
        let request = build_request(slot, &candidates, max_items, now);
        match ask_model(config, system, &request).await {
            Ok(answer) => match parse_picks(&answer, candidates.len(), max_items) {
                Some(picks) => (picks, true),
                None => {
                    tracing::warn!(
                        slot = slot_name,
                        "news model answer unusable; using fallback"
                    );
                    (fallback_picks(&candidates, max_items), false)
                }
            },
            Err(e) => {
                tracing::warn!(
                    slot = slot_name,
                    "news model call failed: {e:#}; using fallback"
                );
                (fallback_picks(&candidates, max_items), false)
            }
        }
    };

    // 6. source health (failure counting / bans) — by code, not by a report.
    let stamp = now.to_rfc3339();
    let outcome = news::update_data(config, rules, |d| {
        Ok(news::record_results(d, rules, &results, &stamp))
    });
    let mut footer = String::new();
    if !failed.is_empty() {
        let _ = write!(
            footer,
            "⚠️ 源状态：{} 个源这次没抓到（{}）",
            failed.len(),
            failed.join("、")
        );
    }
    match outcome {
        Ok(o) if !o.newly_banned.is_empty() => {
            if let Err(e) = news::reconcile(config) {
                tracing::warn!("news reconcile after ban failed: {e:#}");
            }
            let _ = write!(
                footer,
                "{}新封禁：{}",
                if footer.is_empty() { "⚠️ " } else { "；" },
                o.newly_banned.join("、")
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("recording news source results failed: {e:#}"),
    }

    let picked: Vec<&Item> = chosen.iter().map(|c| &candidates[c.index]).collect();
    record_pushed(&history, &picked, slot_name, now)?;
    Ok(render(
        slot,
        &local_time_label(rules, now),
        quotes.as_deref(),
        &candidates,
        &chosen,
        model_ok,
        &footer,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(source: &str, title: &str, link: &str) -> Item {
        Item {
            source: source.into(),
            title: title.into(),
            link: link.into(),
            published: None,
            text: String::new(),
            official: false,
        }
    }

    #[test]
    fn parse_feed_reads_rss_items_with_cdata_entities_and_dates() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>Chan</title>
<item><title><![CDATA[Drone &amp; EW update]]></title><link>https://a.example.com/1?utm_source=x</link>
<pubDate>Thu, 24 Sep 2026 22:10:00 +0000</pubDate><description>&lt;p&gt;Body&nbsp;text&lt;/p&gt;</description>
<source url="https://pub.example.com">Reuters</source></item>
<item><title>Second</title><link>https://a.example.com/2</link></item>
</channel></rss>"#;
        let items = parse_feed(xml, "Feed");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Drone & EW update");
        assert_eq!(items[0].link, "https://a.example.com/1?utm_source=x");
        assert_eq!(items[0].source, "Feed·Reuters");
        assert_eq!(
            items[0].published.unwrap().to_rfc3339(),
            "2026-09-24T22:10:00+00:00"
        );
        assert_eq!(items[0].text, "Body text");
        assert!(items[1].published.is_none());
    }

    #[test]
    fn parse_feed_reads_atom_entries_and_alternate_links() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>X</title>
<entry><title type="html">Atom &lt;b&gt;post&lt;/b&gt;</title>
<link rel="replies" href="https://b.example.com/c"/><link rel="alternate" href="https://b.example.com/p"/>
<updated>2026-09-25T01:00:00Z</updated><summary>Short</summary></entry></feed>"#;
        let items = parse_feed(xml, "Blog");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Atom post");
        assert_eq!(items[0].link, "https://b.example.com/p");
        assert_eq!(items[0].text, "Short");
    }

    #[test]
    fn parse_telegram_extracts_posts_links_and_times() {
        let html = r#"<div class="tgme_widget_message_wrap"><div class="tgme_widget_message" data-post="wartranslated/123">
<div class="tgme_widget_message_text js-message_text" dir="auto">A logistics facility is on fire<br/>in occupied Luhansk &amp; more</div>
<a class="tgme_widget_message_date"><time datetime="2026-09-25T01:02:03+00:00">01:02</time></a></div></div>
<div class="tgme_widget_message" data-post="wartranslated/124"><div class="tgme_widget_message_text">short</div></div>"#;
        let items = parse_telegram(html, "wartranslated", "@wartranslated");
        assert_eq!(items.len(), 1, "posts under 12 chars are skipped");
        assert_eq!(items[0].link, "https://t.me/wartranslated/123");
        assert_eq!(
            items[0].text,
            "A logistics facility is on fire in occupied Luhansk & more"
        );
        assert_eq!(
            items[0].published.unwrap().to_rfc3339(),
            "2026-09-25T01:02:03+00:00"
        );
    }

    #[test]
    fn parse_polymarket_keeps_only_big_moves_with_volume() {
        let json = serde_json::json!([{"slug": "houthi-tanker", "markets": [
            {"question": "Houthis seize a tanker?", "oneDayPriceChange": -0.28, "volume24hr": 90000,
             "outcomePrices": "[\"0.12\", \"0.88\"]"},
            {"question": "Small move", "oneDayPriceChange": 0.02, "volume24hr": 900_000},
            {"question": "Thin market", "oneDayPriceChange": 0.5, "volume24hr": 100}
        ]}]);
        let items = parse_polymarket(&json, "Polymarket", Utc::now());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].link, "https://polymarket.com/event/houthi-tanker");
        assert!(items[0].text.contains("当前概率 12%"));
        assert!(items[0].text.contains("-28 个百分点"));
    }

    #[test]
    fn decode_entities_handles_cjk_after_ampersand_and_numeric_entities() {
        assert_eq!(
            decode_entities("A&中文测试一二三四五；&amp;B"),
            "A&中文测试一二三四五；&B"
        );
        assert_eq!(
            decode_entities("&#20013;&#x6587; &lt;b&gt; &unknown; &"),
            "中文 <b> &unknown; &"
        );
    }

    #[test]
    fn url_and_title_keys_ignore_tracking_and_formatting() {
        assert_eq!(
            url_key("https://www.Example.com/a/b/?utm_source=x&id=7#frag"),
            url_key("http://example.com/a/b?id=7")
        );
        assert_eq!(
            title_key("Oil jumps 3% — OPEC cut!"),
            title_key("oil jumps 3%, OPEC cut")
        );
    }

    #[test]
    fn dedupe_drops_history_and_cross_source_duplicates() {
        let pushed_urls: HashSet<String> = [url_key("https://a.example.com/old")].into();
        let pushed_titles: HashSet<String> = [title_key("Pushed yesterday")].into();
        let items = vec![
            item("A", "Fresh story", "https://a.example.com/1"),
            item("B", "Fresh story", "https://b.example.com/9"),
            item("A", "Old link", "https://a.example.com/old?utm_medium=rss"),
            item("C", "Pushed yesterday", "https://c.example.com/x"),
            item("C", "Another", "https://a.example.com/1/"),
        ];
        let kept = dedupe(items, &pushed_urls, &pushed_titles);
        assert_eq!(
            kept.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["Fresh story"]
        );
    }

    #[test]
    fn select_from_source_filters_keywords_age_and_caps() {
        let now = Utc::now();
        let mut items: Vec<Item> = (0..20)
            .map(|i| Item {
                published: Some(now - chrono::Duration::minutes(i)),
                ..item(
                    "S",
                    &format!("OIL price move {i}"),
                    &format!("https://s.example.com/{i}"),
                )
            })
            .collect();
        items.push(Item {
            published: Some(now - chrono::Duration::hours(FRESH_HOURS + 1)),
            ..item("S", "oil stale", "https://s.example.com/old")
        });
        items.push(item("S", "football score", "https://s.example.com/f"));
        let src = Source {
            url: "https://s.example.com/feed".into(),
            filter: vec!["oil".into()],
            ..Source::default()
        };
        let kept = select_from_source(items, &src, now);
        assert_eq!(kept.len(), PER_SOURCE_CAP);
        assert_eq!(kept[0].title, "OIL price move 0");
        assert!(kept
            .iter()
            .all(|i| i.title.to_lowercase().contains("oil") && i.title != "oil stale"));
    }

    #[test]
    fn parse_picks_validates_ids_and_tolerates_fences() {
        let answer = "```json\n{\"items\":[{\"id\":1,\"category\":\"国际\",\"title\":\"标题\",\"summary\":\"摘要\"},\
                      {\"id\":1,\"category\":\"x\"},{\"id\":99,\"category\":\"x\"},{\"id\":0,\"category\":\"AI\",\"title\":\"t\"}]}\n```";
        let picks = parse_picks(answer, 3, 10).unwrap();
        assert_eq!(
            picks.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert!(parse_picks("sorry, cannot help", 3, 10).is_none());
        assert!(parse_picks("{\"items\":[{\"id\":7}]}", 3, 10).is_none());
    }

    #[test]
    fn fallback_round_robins_sources_and_skips_official_media() {
        let mut c = vec![
            item("A", "a1", "https://a.example.com/1"),
            item("A", "a2", "https://a.example.com/2"),
            item("B", "b1", "https://b.example.com/1"),
        ];
        c.push(Item {
            official: true,
            ..item("X", "propaganda", "https://news.cn/1")
        });
        let picks = fallback_picks(&c, 3);
        assert_eq!(
            picks.iter().map(|p| p.index).collect::<Vec<_>>(),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn render_takes_links_from_items_not_from_the_model() {
        let slot = Slot {
            name: "早报".into(),
            time: "07:00".into(),
            ..Slot::default()
        };
        let candidates = vec![
            item("@kyiv", "Original", "https://t.me/kyiv/5"),
            Item {
                official: true,
                ..item("新华网", "官方", "https://www.news.cn/x")
            },
        ];
        let chosen = vec![
            Chosen {
                index: 0,
                category: "俄乌战场".into(),
                title: "中文[标题]".into(),
                summary: "摘要".into(),
            },
            Chosen {
                index: 1,
                category: "两岸三地".into(),
                title: "官方说法".into(),
                summary: String::new(),
            },
        ];
        let text = render(
            &slot,
            "09-25 周四 07:00",
            Some("📈 行情\n"),
            &candidates,
            &chosen,
            true,
            "⚠️ 源状态：x",
        );
        assert!(text.contains("• [中文(标题)](https://t.me/kyiv/5) — 摘要（@kyiv）"));
        assert!(text.contains("🏛官方口径 [官方说法](https://www.news.cn/x)"));
        assert!(text.contains("**【俄乌战场】**") && text.contains("📈 行情"));
        assert!(text.ends_with("⚠️ 源状态：x"));
    }

    #[test]
    fn quotes_parse_and_render() {
        let yahoo = serde_json::json!({"chart": {"result": [{"meta": {"regularMarketPrice": 4330.3},
            "indicators": {"quote": [{"close": [4200.0, 4297.9, null, 4330.3]}]}}]}});
        let (price, change) = parse_yahoo(&yahoo).unwrap();
        assert!((price - 4330.3).abs() < 1e-9);
        assert!((change.unwrap() - (4330.3 / 4297.9 - 1.0) * 100.0).abs() < 1e-9);

        let boc_html =
            "<tr><td>美元</td><td>670.37</td><td>670.37</td><td>673.19</td><td>673.19</td>\
                        <td>674.89</td><td>2026/09/25 09:45:37</td><td>09:45:37</td></tr>";
        let boc = parse_boc(boc_html).unwrap();
        assert_eq!(
            (boc.buy.as_str(), boc.sell.as_str(), boc.middle.as_str()),
            ("670.37", "673.19", "674.89")
        );

        let quotes = vec![Quote {
            label: "黄金",
            decimals: 1,
            price: 4330.3,
            change_pct: Some(0.754),
        }];
        let text = render_quotes(&quotes, Some(&boc));
        assert!(text.contains("🥇 黄金 4,330.3（+0.75%）"), "{text}");
        assert!(text.contains("现汇买入 670.37 ｜ 现汇卖出 673.19 ｜ 折算价 674.89（09:45:37）"));
        assert_eq!(fmt_number(84562.8, 0), "84,563");
        assert_eq!(fmt_number(-1234.5, 1), "-1,234.5");
    }

    #[test]
    fn history_roundtrip_excludes_pushed_items() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_history(dir.path()).unwrap();
        let now = Utc::now();
        let a = item("A", "Story one", "https://a.example.com/1");
        record_pushed(&conn, &[&a], "早报", now).unwrap();
        let (urls, titles) = pushed_since(&conn, now - chrono::Duration::days(1)).unwrap();
        let kept = dedupe(
            vec![
                item("B", "story ONE!", "https://b.example.com/x"),
                item("C", "New", "https://a.example.com/1?utm_x=1"),
            ],
            &urls,
            &titles,
        );
        assert!(kept.is_empty());
    }

    #[test]
    fn kind_detection_and_display_names() {
        assert!(
            matches!(kind_of("https://t.me/s/wartranslated"), Kind::Telegram(c) if c == "wartranslated")
        );
        assert!(
            matches!(kind_of("https://t.me/KyivIndependent_official"), Kind::Telegram(c) if c == "KyivIndependent_official")
        );
        assert!(matches!(
            kind_of("https://gamma-api.polymarket.com/events?tag_slug=world"),
            Kind::Polymarket
        ));
        assert!(matches!(
            kind_of("https://hn.algolia.com/api/v1/search?tags=front_page"),
            Kind::HackerNews
        ));
        assert!(matches!(
            kind_of("https://www.techmeme.com/feed.xml"),
            Kind::Auto
        ));
        assert_eq!(
            display_name(&Source {
                url: "https://t.me/s/tnews365".into(),
                ..Source::default()
            }),
            "@tnews365"
        );
        assert_eq!(
            display_name(&Source {
                url: "https://t.me/s/x".into(),
                name: "竹新社".into(),
                ..Source::default()
            }),
            "竹新社"
        );
        assert!(is_official(
            "https://www.news.cn/politics/2026-09/25/c_1.htm"
        ));
        assert!(!is_official("https://www.voachinese.com/a/1.html"));
    }
}
