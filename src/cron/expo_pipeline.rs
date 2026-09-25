//! elfClaw 2026-09-25: daily expo push (`kind = "expo"` slots).
//!
//! Upcoming expos in Sydney / Melbourne / Brisbane / Gold Coast / Adelaide
//! within `WINDOW_DAYS` days. Each expo is announced three times: when it is
//! first found, about a month before it opens, and in the week before. Code
//! fetches the pages and keeps the state (`state/expo.db`); the model only
//! does text work:
//!
//! 1. fetch every source: schema.org Event JSON-LD (Eventbrite and others) is
//!    read by code; other pages (venue calendars, EventsEye, expo sites) are
//!    reduced to plain text (through cf-crawler's browser for `browser = true`);
//! 2. one model call picks the expos out of that material, classifies them
//!    and reads dates from the text; code checks city, dates and window, and
//!    takes each link from the page's own anchors or the structured data —
//!    never from the model;
//! 3. merge with the database (same city, start within a day, similar name);
//! 4. for expos without ticket info, code fetches their own page and the
//!    model is asked once for the price and the free-ticket routes it states;
//! 5. code decides today's notices and renders the message.

use crate::config::Config;
use crate::cron::news::{self, NewsRules, Slot, Source, SourceResult};
use crate::cron::news_pipeline::{
    ask_model, decode_entities, display_name, get_text, http_client, local_time_label,
    looks_like_feed, parse_feed, plain_text,
};
use crate::security::SecurityPolicy;
use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

/// Only expos starting within this many days are collected.
const WINDOW_DAYS: i64 = 60;
const MONTH_NOTICE_DAYS: i64 = 30;
const WEEK_NOTICE_DAYS: i64 = 7;
/// An "expo" running longer than this is an exhibition / attraction, not a show.
const MAX_RUN_DAYS: i64 = 21;
const PAGE_TEXT_CHARS: usize = 16_000;
const DETAIL_TEXT_CHARS: usize = 4_000;
/// Expos whose ticket info is looked up per run (one detail page each).
const MAX_ENRICH: usize = 12;
const FETCH_CONCURRENCY: usize = 5;
/// Venue calendars and ad boards can be slow (141go161.com took 30 s+).
const PAGE_TIMEOUT_SECS: u64 = 60;
/// Rows are kept this long after the expo ended, then deleted.
const KEEP_DAYS_AFTER_END: i64 = 30;
/// A page with this many structured events needs no text extraction.
const STRUCTURED_ENOUGH: usize = 3;
pub const CITIES: &[&str] = &["悉尼", "墨尔本", "布里斯班", "黄金海岸", "阿德莱德"];
/// The kinds of expo the user follows; anything the model files elsewhere
/// (or under "其他") is dropped by code.
pub const CATEGORIES: &[&str] = &[
    "汽车",
    "建筑家居",
    "商品博览",
    "电子科技",
    "工业制造",
    "图书",
    "游戏",
    "动漫同人",
    "收藏潮玩",
    "安防",
    "博彩",
    "航空航天",
    "防务军工",
    "成人",
];
/// Words too generic to tell two expo names apart.
const GENERIC_WORDS: &[&str] = &[
    "the",
    "and",
    "expo",
    "show",
    "australia",
    "australian",
    "festival",
    "exhibition",
    "conference",
    "fair",
    "sydney",
    "melbourne",
    "brisbane",
    "adelaide",
    "gold",
    "coast",
    "for",
    "trade",
    "international",
];

// ── page material ────────────────────────────────────────────────────────

/// A schema.org Event read from a page's JSON-LD.
#[derive(Debug, Clone, PartialEq)]
pub struct LdEvent {
    pub name: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub venue: String,
    pub locality: String,
    pub url: String,
    pub price: String,
}

fn ld_date(v: Option<&Value>) -> Option<NaiveDate> {
    let s = v?.as_str()?;
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

fn first_object(v: Option<&Value>) -> Option<&Value> {
    match v? {
        Value::Array(list) => list.first(),
        other => Some(other),
    }
}

fn ld_price(offers: Option<&Value>) -> String {
    let Some(o) = first_object(offers) else {
        return String::new();
    };
    let num = |k: &str| {
        o.get(k).and_then(|v| {
            v.as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
    };
    let currency = o.get("priceCurrency").and_then(Value::as_str).unwrap_or("");
    let fmt = |x: f64| format!("{currency} {x:.0}").trim().to_string();
    match (num("lowPrice"), num("highPrice"), num("price")) {
        (Some(lo), Some(hi), _) if hi > lo => format!("{}–{hi:.0}", fmt(lo)),
        (Some(x), _, _) | (None, _, Some(x)) if x == 0.0 => "免费".to_string(),
        (Some(x), _, _) | (None, _, Some(x)) => fmt(x),
        _ => String::new(),
    }
}

fn resolve(base: &reqwest::Url, href: &str) -> Option<String> {
    let url = base.join(href.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// Every schema.org `*Event` in the page's JSON-LD blocks, de-duplicated.
pub fn parse_ld_events(html: &str, base: &reqwest::Url) -> Vec<LdEvent> {
    static LD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<script[^>]*application/ld\+json[^>]*>(.*?)</script>"#)
            .expect("valid regex")
    });
    let mut out: Vec<LdEvent> = Vec::new();
    for block in LD.captures_iter(html) {
        let Ok(root) = serde_json::from_str::<Value>(block[1].trim()) else {
            continue;
        };
        let mut stack = vec![&root];
        while let Some(v) = stack.pop() {
            match v {
                Value::Array(list) => stack.extend(list),
                Value::Object(map) => {
                    for key in ["@graph", "itemListElement", "item", "subEvent"] {
                        if let Some(child) = map.get(key) {
                            stack.push(child);
                        }
                    }
                    let is_event = match map.get("@type") {
                        Some(Value::String(t)) => t.ends_with("Event"),
                        Some(Value::Array(ts)) => ts
                            .iter()
                            .any(|t| t.as_str().is_some_and(|t| t.ends_with("Event"))),
                        _ => false,
                    };
                    if !is_event {
                        continue;
                    }
                    let name = map
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| decode_entities(n).trim().to_string())
                        .unwrap_or_default();
                    let Some(start) = ld_date(map.get("startDate")) else {
                        continue;
                    };
                    if name.is_empty() {
                        continue;
                    }
                    let end = ld_date(map.get("endDate"))
                        .filter(|e| *e >= start)
                        .unwrap_or(start);
                    let location = first_object(map.get("location"));
                    let venue = location
                        .and_then(|l| l.get("name"))
                        .and_then(Value::as_str)
                        .map(|n| decode_entities(n).trim().to_string())
                        .unwrap_or_default();
                    let locality = location
                        .and_then(|l| l.get("address"))
                        .and_then(|a| match a {
                            Value::String(s) => Some(s.as_str()),
                            other => other.get("addressLocality").and_then(Value::as_str),
                        })
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let url = map
                        .get("url")
                        .and_then(Value::as_str)
                        .and_then(|u| resolve(base, u))
                        .unwrap_or_default();
                    let event = LdEvent {
                        name,
                        start,
                        end,
                        venue,
                        locality,
                        url,
                        price: ld_price(map.get("offers")),
                    };
                    if !out
                        .iter()
                        .any(|e| e.name == event.name && e.start == event.start)
                    {
                        out.push(event);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `(text, absolute url)` of every link on the page.
pub fn parse_anchors(html: &str, base: &reqwest::Url) -> Vec<(String, String)> {
    static A: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<a\s[^>]*?href\s*=\s*["']([^"'#][^"']*)["'][^>]*>(.*?)</a>"#)
            .expect("valid regex")
    });
    A.captures_iter(html)
        .filter_map(|c| {
            let text = plain_text(&c[2], 200);
            let url = resolve(base, &decode_entities(&c[1]))?;
            (text.chars().count() >= 4).then_some((text, url))
        })
        .collect()
}

/// Visible text of an HTML page (scripts, styles and inline SVG removed).
pub fn page_text(html: &str, max: usize) -> String {
    static HIDDEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?is)<script\b.*?</script>|<style\b.*?</style>|<noscript\b.*?</noscript>|<svg\b.*?</svg>",
        )
        .expect("valid regex")
    });
    plain_text(&HIDDEN.replace_all(html, " "), max)
}

fn name_tokens(name: &str) -> HashSet<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| {
            w.chars().count() >= 3
                && !GENERIC_WORDS.contains(w)
                && !(w.len() == 4 && w.chars().all(|c| c.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect()
}

/// Share of the smaller token set found in the other one (0 when either is empty).
fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let smaller = a.len().min(b.len());
    if smaller == 0 {
        return 0.0;
    }
    a.intersection(b).count() as f64 / smaller as f64
}

/// The page link whose text best matches `name`, if it matches well.
pub fn best_anchor(anchors: &[(String, String)], name: &str) -> Option<String> {
    let wanted = name_tokens(name);
    if wanted.is_empty() {
        return None;
    }
    anchors
        .iter()
        .map(|(text, url)| {
            let got = name_tokens(text);
            let score = wanted.intersection(&got).count() as f64 / wanted.len() as f64;
            (score, url)
        })
        .filter(|(score, _)| *score >= 0.6)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, url)| url.clone())
}

pub(super) struct Page {
    pub(super) src: Source,
    pub(super) name: String,
    pub(super) text: String,
    pub(super) anchors: Vec<(String, String)>,
    pub(super) events: Vec<LdEvent>,
}

async fn get_page(client: &reqwest::Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(PAGE_TIMEOUT_SECS))
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

pub(super) async fn fetch_page(
    client: &reqwest::Client,
    security: &SecurityPolicy,
    src: Source,
) -> Result<Page> {
    let name = display_name(&src);
    let (text, anchors, events) = if src.local_browser {
        let page = crate::tools::local_browser::fetch(security, std::slice::from_ref(&src.url))
            .await?
            .pop()
            .context("本地浏览器没有返回结果")?
            .map_err(|e| anyhow::anyhow!(e))?;
        let base =
            reqwest::Url::parse(&page.final_url).or_else(|_| reqwest::Url::parse(&src.url))?;
        let events = parse_ld_events(&page.html, &base);
        let text = if events.len() >= STRUCTURED_ENOUGH {
            String::new()
        } else {
            page_text(&page.html, PAGE_TEXT_CHARS)
        };
        (text, parse_anchors(&page.html, &base), events)
    } else if src.tinyfish {
        let page = crate::cron::tinyfish::fetch(client, std::slice::from_ref(&src.url))
            .await?
            .pop()
            .context("TinyFish 没有返回结果")?
            .map_err(|e| anyhow::anyhow!(e))?;
        let anchors = page
            .links
            .iter()
            .map(|l| (crate::cron::tinyfish::slug_text(l), l.clone()))
            .collect();
        (
            crate::util::truncate_with_ellipsis(&page.text, PAGE_TEXT_CHARS),
            anchors,
            Vec::new(),
        )
    } else if src.browser {
        let v = crate::tools::cf_crawler::scrape_page(
            security,
            &src.url,
            "upcoming events with dates",
            "article",
            "edge_browser",
        )
        .await?;
        let mut anchors = Vec::new();
        let mut titles = Vec::new();
        for it in v
            .get("items")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(title), Some(url)) = (
                it.get("title").and_then(Value::as_str),
                it.get("url").and_then(Value::as_str),
            ) else {
                continue;
            };
            let title = plain_text(title, 200);
            if url.starts_with("http") && title.chars().count() >= 4 {
                titles.push(title.clone());
                anchors.push((title, url.to_string()));
            }
        }
        let markdown = v
            .get("markdown")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = plain_text(
            &format!("{markdown}\n{}", titles.join(" | ")),
            PAGE_TEXT_CHARS,
        );
        (text, anchors, Vec::new())
    } else {
        let body = get_page(client, &src.url).await?;
        let base = reqwest::Url::parse(&src.url)?;
        let events = parse_ld_events(&body, &base);
        let text = if events.len() >= STRUCTURED_ENOUGH {
            String::new()
        } else {
            page_text(&body, PAGE_TEXT_CHARS)
        };
        // A feed's entries link with <link>, not <a>: use them as anchors.
        let anchors = if looks_like_feed(&body) {
            parse_feed(&body, "")
                .into_iter()
                .map(|it| (it.title, it.link))
                .collect()
        } else {
            parse_anchors(&body, &base)
        };
        (text, anchors, events)
    };
    anyhow::ensure!(
        !events.is_empty() || text.chars().count() >= 200,
        "页面没有可用内容（可能需要浏览器渲染）"
    );
    Ok(Page {
        src,
        name,
        text,
        anchors,
        events,
    })
}

// ── expos ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Expo {
    /// Database row id; 0 before the expo is stored.
    pub id: i64,
    pub name: String,
    pub name_zh: String,
    pub category: String,
    pub city: String,
    pub venue: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub url: String,
    pub price: String,
    pub free_ticket: String,
    /// Ticket info has been looked up (whether or not anything was found).
    pub enriched: bool,
    pub found_at: Option<String>,
    pub month_at: Option<String>,
    pub week_at: Option<String>,
}

/// Same expo seen twice (other source, other wording of the name).
pub fn same_expo(a: &Expo, b: &Expo) -> bool {
    if a.city != b.city || (a.start - b.start).num_days().abs() > 1 {
        return false;
    }
    let (ta, tb) = (name_tokens(&a.name), name_tokens(&b.name));
    if ta.is_empty() || tb.is_empty() {
        return a.name.eq_ignore_ascii_case(&b.name);
    }
    overlap(&ta, &tb) >= 0.5
}

#[derive(Debug, Deserialize)]
struct Found {
    #[serde(default, rename = "ref")]
    reference: String,
    #[serde(default)]
    src: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    name_zh: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    venue: String,
    #[serde(default)]
    start: String,
    #[serde(default)]
    end: String,
}

#[derive(Debug, Deserialize)]
struct FoundList {
    events: Vec<Found>,
}

fn json_object(answer: &str) -> Option<&str> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    answer.get(start..=end)
}

fn in_window(start: NaiveDate, end: NaiveDate, today: NaiveDate) -> bool {
    end >= today
        && (start - today).num_days() <= WINDOW_DAYS
        && (end - start).num_days() <= MAX_RUN_DAYS
}

/// Structured events handed to the model: in the window, one per name+date.
fn structured_candidates(pages: &[Page], today: NaiveDate) -> Vec<(usize, LdEvent)> {
    let mut out: Vec<(usize, LdEvent)> = Vec::new();
    for (p, page) in pages.iter().enumerate() {
        for e in &page.events {
            if in_window(e.start, e.end, today)
                && !out
                    .iter()
                    .any(|(_, o)| o.name == e.name && o.start == e.start)
            {
                out.push((p, e.clone()));
            }
        }
    }
    out
}

/// Model answer → validated expos. Dates, venue and link of structured
/// events come from the data; links of text finds come from the page's
/// anchors (or the page itself). Anything outside the cities / window is
/// dropped; duplicates within the answer are merged.
fn resolve_found(
    answer: &str,
    pages: &[Page],
    structured: &[(usize, LdEvent)],
    today: NaiveDate,
) -> Option<Vec<Expo>> {
    let list: FoundList = serde_json::from_str(json_object(answer)?).ok()?;
    let mut out: Vec<Expo> = Vec::new();
    for f in list.events {
        let city = f.city.trim();
        let category = f.category.trim();
        if !CITIES.contains(&city) || !CATEGORIES.contains(&category) {
            continue;
        }
        let mut expo = Expo {
            name_zh: f.name_zh.trim().to_string(),
            category: category.to_string(),
            city: city.to_string(),
            ..Expo::default()
        };
        if let Some(i) = f
            .reference
            .trim()
            .strip_prefix('E')
            .and_then(|n| n.parse::<usize>().ok())
        {
            let Some((p, e)) = structured.get(i) else {
                continue;
            };
            expo.name.clone_from(&e.name);
            expo.start = e.start;
            expo.end = e.end;
            expo.venue.clone_from(&e.venue);
            expo.price.clone_from(&e.price);
            expo.url = if e.url.is_empty() {
                pages[*p].src.url.clone()
            } else {
                e.url.clone()
            };
        } else if let Some(i) = f
            .src
            .trim()
            .strip_prefix('S')
            .and_then(|n| n.parse::<usize>().ok())
        {
            let Some(page) = pages.get(i) else {
                continue;
            };
            let Ok(start) = NaiveDate::parse_from_str(f.start.trim(), "%Y-%m-%d") else {
                continue;
            };
            let end = NaiveDate::parse_from_str(f.end.trim(), "%Y-%m-%d")
                .ok()
                .filter(|e| *e >= start)
                .unwrap_or(start);
            expo.name = f.name.trim().to_string();
            expo.start = start;
            expo.end = end;
            expo.venue = f.venue.trim().to_string();
            expo.url =
                best_anchor(&page.anchors, &expo.name).unwrap_or_else(|| page.src.url.clone());
        } else {
            continue;
        }
        if expo.name.is_empty() || !in_window(expo.start, expo.end, today) {
            continue;
        }
        if !out.iter().any(|o| same_expo(o, &expo)) {
            out.push(expo);
        }
    }
    Some(out)
}

const EXTRACT_SYSTEM: &str = "你是会展信息编辑，为住在悉尼的读者整理澳洲即将举办的展会。
规则：
1. 只从给出的材料里提取，不要编造展会、日期或地点，不要写链接。
2. 只要有一定规模、在展馆或会展中心举办的展会，并且属于这些类别（category 只能从中选一个）：
   汽车（车展、四驱房车、船艇、摩托）；建筑家居（建筑、装修、家居展）；商品博览（综合商品、礼品、美食美酒等消费展）；电子科技（电子、IT、音响、智慧城市、新能源技术）；工业制造（工业、矿业能源、物流、清洁设备等贸易展）；图书；游戏；动漫同人（漫展、Comic Con、同人祭）；收藏潮玩（卡牌、玩具、收藏品展）；安防；博彩；航空航天（航展）；防务军工（武器、军火、防务展）；成人（成人生活方式展，如 Sexpo、SexEx，是正规展会，照常收录）。
   不属于这些类别的一律不要，例如：演唱会、演出、体育赛事、晚宴、培训课、讲座、学术会议、招聘会、婚礼展、房产投资推介、巡回路演、快闪店和特卖会、小型集市、社区/养老/残障/身心灵类博览会、医疗健康展、长期展览；在酒吧、餐厅、教堂、图书馆、社区中心、俱乐部或公司门店里办的小活动也不要。
3. 城市只能填：悉尼、墨尔本、布里斯班、黄金海岸、阿德莱德（郊区归到所属城市，例如 Parramatta、Sydney Olympic Park 属于悉尼，Wayville 属于阿德莱德）；其他城市的展会不要。
4. 结构化条目（E 开头）的日期和地点已由程序读出，用 ref 引用编号即可；网页文字（S 开头）里找到的，写 src 编号和 name、venue、start、end。日期用 YYYY-MM-DD；材料没写年份时取今天之后最近的那一次；只有月份没有具体日期的不要。同一个展会在多处出现只写一次，优先用结构化条目。
5. name 保留原文名称；name_zh 写简短中文名。
6. 只输出 JSON，不要其他文字：{\"events\":[{\"ref\":\"E3\",\"name_zh\":\"…\",\"category\":\"…\",\"city\":\"…\"},{\"src\":\"S2\",\"name\":\"…\",\"name_zh\":\"…\",\"category\":\"…\",\"city\":\"…\",\"venue\":\"…\",\"start\":\"2026-10-13\",\"end\":\"2026-10-15\"}]}";

fn build_extract_request(
    pages: &[Page],
    structured: &[(usize, LdEvent)],
    today: NaiveDate,
) -> String {
    let last = today + chrono::Duration::days(WINDOW_DAYS);
    let mut out = format!(
        "今天是 {today}。请找出 {today} 到 {last} 之间开幕（或正在举办）的展会。\n\n结构化条目：\n"
    );
    for (i, (p, e)) in structured.iter().enumerate() {
        let _ = writeln!(
            out,
            "[E{i}] {} | {}~{} | {} {} | 来自 {}",
            e.name, e.start, e.end, e.venue, e.locality, pages[*p].name
        );
    }
    out.push_str("\n网页文字：\n");
    for (i, page) in pages.iter().enumerate() {
        if page.text.is_empty() {
            continue;
        }
        let _ = write!(
            out,
            "\n[S{i}] {}（{}）\n{}\n",
            page.name, page.src.url, page.text
        );
    }
    out
}

// ── ticket info ──────────────────────────────────────────────────────────

const TICKET_SYSTEM: &str = "你根据展会官网页面的文字，整理门票价格和免费入场的办法。\n\
规则：\n\
1. price：页面写明的票价，简短中文，例如「成人 $35，儿童 $15」「行业观众免费，公众 $30」；页面没写就填空字符串。\
不要编造价格。\n\
2. free_ticket：页面写明的免费入场办法，例如行业观众网上预登记免费、某日免费、儿童免费、做志愿者、\
抽奖送票；页面没写，但它明显是只对业内人士开放的贸易展，写「贸易展通常业内人士网上预登记可免费入场（页面未写明）」；\
其他情况填空字符串。\n\
3. 每项不超过 60 字。只输出 JSON：{\"items\":[{\"id\":编号,\"price\":\"…\",\"free_ticket\":\"…\"}]}";

#[derive(Debug, Deserialize)]
struct Ticket {
    id: i64,
    #[serde(default)]
    price: String,
    #[serde(default)]
    free_ticket: String,
}

#[derive(Debug, Deserialize)]
struct TicketList {
    items: Vec<Ticket>,
}

/// Expo pages to look up this run: not yet looked up, due today first.
fn enrich_targets(expos: &[Expo], today: NaiveDate) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..expos.len()).filter(|&i| !expos[i].enriched).collect();
    idx.sort_by_key(|&i| (due_notice(&expos[i], today).is_none(), expos[i].start));
    idx.truncate(MAX_ENRICH);
    idx
}

async fn fetch_detail(client: reqwest::Client, url: String) -> (String, String) {
    let Ok(body) = get_text(&client, &url).await else {
        return (String::new(), String::new());
    };
    let price = reqwest::Url::parse(&url)
        .ok()
        .and_then(|base| {
            parse_ld_events(&body, &base)
                .into_iter()
                .find(|e| !e.price.is_empty())
        })
        .map(|e| e.price)
        .unwrap_or_default();
    (price, page_text(&body, DETAIL_TEXT_CHARS))
}

// ── notices ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Found,
    Month,
    Week,
}

pub fn due_notice(e: &Expo, today: NaiveDate) -> Option<Notice> {
    let days = (e.start - today).num_days();
    if e.found_at.is_none() {
        return Some(Notice::Found);
    }
    if e.week_at.is_none() && days <= WEEK_NOTICE_DAYS {
        return Some(Notice::Week);
    }
    if e.month_at.is_none() && days <= MONTH_NOTICE_DAYS && days > WEEK_NOTICE_DAYS {
        return Some(Notice::Month);
    }
    None
}

/// Apply a notice: later milestones already covered by it are marked too,
/// so an expo found 5 days out is not announced again as "next week".
pub fn mark_notice(e: &mut Expo, notice: Notice, today: NaiveDate, stamp: &str) {
    let days = (e.start - today).num_days();
    let set = |slot: &mut Option<String>| {
        if slot.is_none() {
            *slot = Some(stamp.to_string());
        }
    };
    match notice {
        Notice::Found => {
            set(&mut e.found_at);
            if days <= MONTH_NOTICE_DAYS {
                set(&mut e.month_at);
            }
            if days <= WEEK_NOTICE_DAYS {
                set(&mut e.week_at);
            }
        }
        Notice::Month => set(&mut e.month_at),
        Notice::Week => {
            set(&mut e.month_at);
            set(&mut e.week_at);
        }
    }
}

// ── database ─────────────────────────────────────────────────────────────

fn open_db(workspace: &Path) -> Result<rusqlite::Connection> {
    let path = workspace.join("state").join("expo.db");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS expos (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL,
             name_zh TEXT NOT NULL DEFAULT '',
             category TEXT NOT NULL DEFAULT '',
             city TEXT NOT NULL,
             venue TEXT NOT NULL DEFAULT '',
             start_date TEXT NOT NULL,
             end_date TEXT NOT NULL,
             url TEXT NOT NULL,
             price TEXT NOT NULL DEFAULT '',
             free_ticket TEXT NOT NULL DEFAULT '',
             enriched INTEGER NOT NULL DEFAULT 0,
             first_seen TEXT NOT NULL,
             found_at TEXT,
             month_at TEXT,
             week_at TEXT
         );",
    )?;
    Ok(conn)
}

fn date_col(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap_or_default()
}

fn load_upcoming(conn: &rusqlite::Connection, today: NaiveDate) -> Result<Vec<Expo>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, name_zh, category, city, venue, start_date, end_date, url, price,
                free_ticket, enriched, found_at, month_at, week_at
         FROM expos WHERE end_date >= ?1 ORDER BY start_date",
    )?;
    let rows = stmt.query_map([today.to_string()], |r| {
        Ok(Expo {
            id: r.get(0)?,
            name: r.get(1)?,
            name_zh: r.get(2)?,
            category: r.get(3)?,
            city: r.get(4)?,
            venue: r.get(5)?,
            start: date_col(&r.get::<_, String>(6)?),
            end: date_col(&r.get::<_, String>(7)?),
            url: r.get(8)?,
            price: r.get(9)?,
            free_ticket: r.get(10)?,
            enriched: r.get::<_, i64>(11)? != 0,
            found_at: r.get(12)?,
            month_at: r.get(13)?,
            week_at: r.get(14)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn insert_expo(conn: &rusqlite::Connection, e: &Expo, stamp: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO expos (name, name_zh, category, city, venue, start_date, end_date, url,
                            price, first_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            e.name,
            e.name_zh,
            e.category,
            e.city,
            e.venue,
            e.start.to_string(),
            e.end.to_string(),
            e.url,
            e.price,
            stamp
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn save_state(conn: &rusqlite::Connection, e: &Expo) -> Result<()> {
    conn.execute(
        "UPDATE expos SET price = ?2, free_ticket = ?3, enriched = ?4, found_at = ?5,
                          month_at = ?6, week_at = ?7
         WHERE id = ?1",
        rusqlite::params![
            e.id,
            e.price,
            e.free_ticket,
            i64::from(e.enriched),
            e.found_at,
            e.month_at,
            e.week_at
        ],
    )?;
    Ok(())
}

// ── rendering ────────────────────────────────────────────────────────────

const WEEKDAYS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

fn fmt_day(d: NaiveDate) -> String {
    format!(
        "{}月{}日（{}）",
        d.month(),
        d.day(),
        WEEKDAYS[d.weekday().num_days_from_monday() as usize]
    )
}

pub fn fmt_dates(e: &Expo, today: NaiveDate) -> String {
    let range = if e.end == e.start {
        fmt_day(e.start)
    } else if e.end.month() == e.start.month() {
        format!("{}–{}日", fmt_day(e.start), e.end.day())
    } else {
        format!("{}–{}月{}日", fmt_day(e.start), e.end.month(), e.end.day())
    };
    let days = (e.start - today).num_days();
    let when = match days {
        d if d > 1 => format!("还有{d}天"),
        1 => "明天开幕".to_string(),
        0 => "今天开幕".to_string(),
        _ => "正在举办".to_string(),
    };
    format!("{range} · {when}")
}

fn render_entry(out: &mut String, e: &Expo, today: NaiveDate) {
    let title = if e.name_zh.is_empty() || e.name_zh == e.name {
        e.name.clone()
    } else {
        format!("{}｜{}", e.name, e.name_zh)
    };
    let category = if e.category.is_empty() {
        String::new()
    } else {
        format!(" — {}", e.category)
    };
    let _ = writeln!(
        out,
        "• [{}]({}){category}",
        title.replace('[', "(").replace(']', ")"),
        e.url
    );
    let venue = if e.venue.is_empty() {
        e.city.clone()
    } else {
        format!("{} · {}", e.city, e.venue)
    };
    let _ = writeln!(out, "  📍 {venue}　📅 {}", fmt_dates(e, today));
    let mut ticket = Vec::new();
    if !e.price.is_empty() {
        ticket.push(format!("门票：{}", e.price));
    }
    if !e.free_ticket.is_empty() {
        ticket.push(format!("免费：{}", e.free_ticket));
    }
    if !ticket.is_empty() {
        let _ = writeln!(out, "  🎫 {}", ticket.join("；"));
    }
}

pub fn render(
    slot: &Slot,
    local_time: &str,
    notices: &[(Notice, Expo)],
    tracked: usize,
    today: NaiveDate,
    footer: &str,
) -> String {
    let mut out = format!("🎪 **{}** | {local_time}\n", slot.name);
    for (kind, heading) in [
        (Notice::Found, "🆕 新发现"),
        (Notice::Month, "⏰ 一个月后开展"),
        (Notice::Week, "📅 一周内开展"),
    ] {
        let list: Vec<&Expo> = notices
            .iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, e)| e)
            .collect();
        if list.is_empty() {
            continue;
        }
        let _ = write!(out, "\n**{heading}（{}）**\n", list.len());
        for e in list {
            render_entry(&mut out, e, today);
        }
    }
    if notices.is_empty() {
        let _ = writeln!(out, "\n今天没有需要通知的展会。");
    }
    let _ = write!(out, "\n正在跟踪 {WINDOW_DAYS} 天内的展会 {tracked} 个。");
    if !footer.is_empty() {
        out.push('\n');
        out.push_str(footer);
    }
    out
}

// ── the job ──────────────────────────────────────────────────────────────

fn local_today(rules: &NewsRules) -> NaiveDate {
    let now = Utc::now();
    rules.tz.parse::<chrono_tz::Tz>().map_or_else(
        |_| now.date_naive(),
        |tz| now.with_timezone(&tz).date_naive(),
    )
}

pub async fn run(
    config: &Config,
    rules: &NewsRules,
    slot: &Slot,
    sources: Vec<Source>,
) -> Result<String> {
    let now = Utc::now();
    let today = local_today(rules);
    let stamp = now.to_rfc3339();
    let client = http_client()?;
    let security = std::sync::Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    // 1. fetch every source.
    let tasks: Vec<_> = sources
        .into_iter()
        .map(|src| {
            let client = client.clone();
            let security = std::sync::Arc::clone(&security);
            async move {
                let url = src.url.clone();
                let label = display_name(&src);
                (url, label, fetch_page(&client, &security, src).await)
            }
        })
        .collect();
    let fetched = stream::iter(tasks)
        .buffered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut pages = Vec::new();
    let mut results = Vec::new();
    let mut failed = Vec::new();
    for (url, label, result) in fetched {
        match result {
            Ok(page) => {
                results.push(SourceResult {
                    url,
                    ok: true,
                    reason: String::new(),
                });
                pages.push(page);
            }
            Err(e) => {
                let reason = crate::util::truncate_with_ellipsis(&format!("{e:#}"), 120);
                tracing::warn!(slot = %slot.name, source = %url, "expo source failed: {reason}");
                failed.push(label);
                results.push(SourceResult {
                    url,
                    ok: false,
                    reason,
                });
            }
        }
    }

    // 2. one model call finds the expos in the material.
    let structured = structured_candidates(&pages, today);
    let mut model_ok = true;
    let found = if pages.is_empty() {
        Vec::new()
    } else {
        let request = build_extract_request(&pages, &structured, today);
        match ask_model(config, EXTRACT_SYSTEM, &request).await {
            Ok(answer) => resolve_found(&answer, &pages, &structured, today).unwrap_or_else(|| {
                tracing::warn!(slot = %slot.name, "expo model answer unusable");
                model_ok = false;
                Vec::new()
            }),
            Err(e) => {
                tracing::warn!(slot = %slot.name, "expo model call failed: {e:#}");
                model_ok = false;
                Vec::new()
            }
        }
    };

    // 3. merge with what is already tracked.
    let conn = open_db(&config.workspace_dir)?;
    let mut expos = load_upcoming(&conn, today)?;
    for mut e in found {
        if expos.iter().any(|o| same_expo(o, &e)) {
            continue;
        }
        e.id = insert_expo(&conn, &e, &stamp)?;
        expos.push(e);
    }
    conn.execute(
        "DELETE FROM expos WHERE end_date < ?1",
        [(today - chrono::Duration::days(KEEP_DAYS_AFTER_END)).to_string()],
    )?;

    // 4. ticket info for expos not looked up yet.
    let listing_urls: HashSet<String> = pages.iter().map(|p| p.src.url.clone()).collect();
    let targets = enrich_targets(&expos, today);
    let detail_tasks: Vec<_> = targets
        .iter()
        .map(|&i| {
            let client = client.clone();
            let url = expos[i].url.clone();
            let skip = listing_urls.contains(&url) || !url.starts_with("http");
            async move {
                if skip {
                    (String::new(), String::new())
                } else {
                    fetch_detail(client, url).await
                }
            }
        })
        .collect();
    let details = stream::iter(detail_tasks)
        .buffered(FETCH_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut request = String::new();
    for (&i, (price, text)) in targets.iter().zip(&details) {
        let e = &mut expos[i];
        if e.price.is_empty() && !price.is_empty() {
            e.price.clone_from(price);
        }
        if !text.is_empty() {
            let _ = write!(
                request,
                "\n[{}] {}（{}，{}~{}）\n结构化票价：{}\n{}\n",
                e.id,
                e.name,
                e.city,
                e.start,
                e.end,
                if e.price.is_empty() { "无" } else { &e.price },
                text
            );
        }
    }
    let mut enrich_ok = true;
    if !request.is_empty() {
        let answer = ask_model(config, TICKET_SYSTEM, &request).await;
        let tickets = answer
            .map_err(|e| tracing::warn!(slot = %slot.name, "expo ticket call failed: {e:#}"))
            .ok()
            .and_then(|a| serde_json::from_str::<TicketList>(json_object(&a)?).ok());
        match tickets {
            Some(list) => {
                for t in list.items {
                    if let Some(e) = expos.iter_mut().find(|e| e.id == t.id) {
                        let price = crate::util::truncate_with_ellipsis(t.price.trim(), 80);
                        if !price.is_empty() {
                            e.price = price;
                        }
                        e.free_ticket =
                            crate::util::truncate_with_ellipsis(t.free_ticket.trim(), 80);
                    }
                }
            }
            None => enrich_ok = false,
        }
    }
    if enrich_ok {
        for &i in &targets {
            expos[i].enriched = true;
        }
    }

    // 5. today's notices.
    let mut notices = Vec::new();
    for e in &mut expos {
        if let Some(notice) = due_notice(e, today) {
            mark_notice(e, notice, today, &stamp);
            notices.push((notice, e.clone()));
        }
    }
    for e in &expos {
        save_state(&conn, e)?;
    }
    notices.sort_by_key(|(_, e)| e.start);

    let outcome = news::update_data(config, rules, |d| {
        Ok(news::record_results(d, rules, &results, &stamp))
    });
    let mut footer = String::new();
    if !model_ok {
        footer.push_str("⚠️ 模型暂时不可用，今天没有提取新展会。\n");
    }
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
        Err(e) => tracing::warn!("recording expo source results failed: {e:#}"),
    }
    let tracked = expos
        .iter()
        .filter(|e| (e.start - today).num_days() <= WINDOW_DAYS)
        .count();
    Ok(render(
        slot,
        &local_time_label(rules, now),
        &notices,
        tracked,
        today,
        footer.trim_end(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn base() -> reqwest::Url {
        reqwest::Url::parse("https://venue.example.com/whats-on/").unwrap()
    }

    fn expo(name: &str, city: &str, start: &str) -> Expo {
        Expo {
            name: name.into(),
            city: city.into(),
            start: d(start),
            end: d(start),
            ..Expo::default()
        }
    }

    fn page(url: &str, anchors: Vec<(String, String)>, events: Vec<LdEvent>) -> Page {
        Page {
            src: Source {
                url: url.into(),
                ..Source::default()
            },
            name: "Venue".into(),
            text: "x".repeat(300),
            anchors,
            events,
        }
    }

    #[test]
    fn parse_ld_events_reads_nested_events_and_offers() {
        let html = r#"<script type="application/ld+json">{"@graph":[{"@type":"ItemList",
            "itemListElement":[{"@type":"ListItem","item":{"@type":"BusinessEvent",
            "name":"Smart City Expo &amp; Forum","startDate":"2026-10-13T09:00:00+11:00",
            "endDate":"2026-10-15","url":"/e/smart-city",
            "location":{"name":"ICC Sydney","address":{"addressLocality":"Sydney"}},
            "offers":[{"lowPrice":"20","highPrice":"80","priceCurrency":"AUD"}]}}]}]}</script>
            <script type="application/ld+json">[{"@type":"Event","name":"Free Day",
            "startDate":"2026-10-20","offers":{"price":0}},{"@type":"Event","name":"No date"}]</script>"#;
        let events = parse_ld_events(html, &base());
        assert_eq!(events.len(), 2);
        let e = &events[0];
        assert_eq!(e.name, "Smart City Expo & Forum");
        assert_eq!((e.start, e.end), (d("2026-10-13"), d("2026-10-15")));
        assert_eq!(e.venue, "ICC Sydney");
        assert_eq!(e.locality, "Sydney");
        assert_eq!(e.url, "https://venue.example.com/e/smart-city");
        assert_eq!(e.price, "AUD 20–80");
        assert_eq!(events[1].price, "免费");
        assert_eq!(events[1].end, events[1].start);
    }

    #[test]
    fn parse_anchors_resolves_links_and_skips_short_text() {
        let html = r##"<a class="x" href="/events/supanova-2026">Supanova <b>Comic Con</b></a>
            <a href="https://other.example.com/a">Go</a><a href="#top">Back to top</a>"##;
        let anchors = parse_anchors(html, &base());
        assert_eq!(
            anchors,
            vec![(
                "Supanova Comic Con".to_string(),
                "https://venue.example.com/events/supanova-2026".to_string()
            )]
        );
    }

    #[test]
    fn page_text_drops_scripts_and_styles() {
        let html =
            "<style>.a{}</style><p>Craft &amp; Quilt Fair 14 Oct</p><script>var x=1;</script>";
        assert_eq!(page_text(html, 100), "Craft & Quilt Fair 14 Oct");
    }

    #[test]
    fn best_anchor_needs_a_close_match() {
        let anchors = vec![
            (
                "Food & Hospitality QLD 2026".to_string(),
                "https://a/food".to_string(),
            ),
            (
                "Craft & Quilt Fair 2026".to_string(),
                "https://a/craft".to_string(),
            ),
        ];
        assert_eq!(
            best_anchor(&anchors, "Food & Hospitality QLD").as_deref(),
            Some("https://a/food")
        );
        assert_eq!(best_anchor(&anchors, "Good Food & Wine Show"), None);
    }

    #[test]
    fn same_expo_matches_reworded_names_in_the_same_city_and_week() {
        let a = expo("Supanova 2026", "阿德莱德", "2026-10-30");
        let b = expo("Supanova Comic Con & Gaming", "阿德莱德", "2026-10-31");
        assert!(same_expo(&a, &b));
        assert!(!same_expo(
            &a,
            &expo("Supanova Comic Con", "布里斯班", "2026-10-30")
        ));
        assert!(!same_expo(
            &a,
            &expo("Supanova 2026", "阿德莱德", "2026-11-06")
        ));
        assert!(!same_expo(
            &expo("Brisbane Disability Expo", "布里斯班", "2026-10-30"),
            &expo("Inked Tattoo Expo Brisbane", "布里斯班", "2026-10-29")
        ));
    }

    #[test]
    fn notices_follow_found_month_week_schedule() {
        let mut e = expo("Security Expo", "悉尼", "2026-11-20");
        let mut today = d("2026-09-25"); // 56 days out
        assert_eq!(due_notice(&e, today), Some(Notice::Found));
        mark_notice(&mut e, Notice::Found, today, "t1");
        assert_eq!(due_notice(&e, today), None);
        today = d("2026-10-20"); // 31 days out
        assert_eq!(due_notice(&e, today), None);
        today = d("2026-10-21"); // 30 days out
        assert_eq!(due_notice(&e, today), Some(Notice::Month));
        mark_notice(&mut e, Notice::Month, today, "t2");
        today = d("2026-11-12"); // 8 days out
        assert_eq!(due_notice(&e, today), None);
        today = d("2026-11-13"); // 7 days out
        assert_eq!(due_notice(&e, today), Some(Notice::Week));
        mark_notice(&mut e, Notice::Week, today, "t3");
        assert_eq!(due_notice(&e, d("2026-11-19")), None);
    }

    #[test]
    fn late_discovery_skips_milestones_already_passed() {
        let mut e = expo("Craft Fair", "布里斯班", "2026-10-14");
        let today = d("2026-10-09"); // 5 days out
        mark_notice(&mut e, Notice::Found, today, "t");
        assert_eq!(due_notice(&e, today), None);
        assert_eq!(due_notice(&e, d("2026-10-12")), None);

        let mut f = expo("Tattoo Expo", "布里斯班", "2026-10-29");
        // 20 days out: the month milestone is covered by the first notice
        mark_notice(&mut f, Notice::Found, today, "t");
        assert_eq!(due_notice(&f, d("2026-10-15")), None);
        assert_eq!(due_notice(&f, d("2026-10-22")), Some(Notice::Week));
    }

    #[test]
    fn resolve_found_takes_dates_and_links_from_data_not_the_model() {
        let today = d("2026-09-25");
        let ld = LdEvent {
            name: "Smart City Expo".into(),
            start: d("2026-10-13"),
            end: d("2026-10-14"),
            venue: "ICC Sydney".into(),
            locality: "Sydney".into(),
            url: "https://eb.example.com/e/1".into(),
            price: String::new(),
        };
        let pages = vec![
            page("https://eb.example.com/d/sydney/expos/", vec![], vec![ld]),
            page(
                "https://bcec.example.com/whats-on/",
                vec![(
                    "Craft & Quilt Fair 2026".into(),
                    "https://bcec.example.com/craft".into(),
                )],
                vec![],
            ),
        ];
        let structured = structured_candidates(&pages, today);
        let answer = r#"```json
{"events":[
 {"ref":"E0","name_zh":"智慧城市展","category":"电子科技","city":"悉尼","start":"2030-01-01"},
 {"src":"S1","name":"Craft & Quilt Fair","name_zh":"手工拼布展","category":"收藏潮玩","city":"布里斯班",
  "venue":"BCEC","start":"2026-10-14","end":"2026-10-17","url":"https://evil.example.com"},
 {"src":"S1","name":"Perth Boat Show","city":"珀斯","start":"2026-10-20"},
 {"src":"S1","name":"Far Future Expo","city":"悉尼","start":"2027-03-01"},
 {"src":"S1","name":"Harry Potter Exhibition","city":"悉尼","start":"2026-09-01","end":"2027-02-01"},
 {"src":"S9","name":"Bad ref","city":"悉尼","start":"2026-10-01"},
 {"ref":"E0","name_zh":"重复","city":"悉尼"}
]}
```"#;
        let found = resolve_found(answer, &pages, &structured, today).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].name, "Smart City Expo");
        assert_eq!(found[0].start, d("2026-10-13"));
        assert_eq!(found[0].url, "https://eb.example.com/e/1");
        assert_eq!(found[0].name_zh, "智慧城市展");
        assert_eq!(found[1].url, "https://bcec.example.com/craft");
        assert_eq!(
            (found[1].start, found[1].end),
            (d("2026-10-14"), d("2026-10-17"))
        );
        assert!(resolve_found("not json", &pages, &structured, today).is_none());
    }

    #[test]
    fn text_find_links_to_source_page_and_unknown_categories_are_dropped() {
        let today = d("2026-09-25");
        let pages = vec![page("https://gccec.example.com/whats-on/", vec![], vec![])];
        let answer = r#"{"events":[{"src":"S0","name":"Biotech Week","category":"电子科技","city":"黄金海岸","start":"2026-10-19"},{"src":"S0","name":"Seniors Expo","category":"其他","city":"黄金海岸","start":"2026-10-19"},{"src":"S0","name":"No Category","city":"黄金海岸","start":"2026-10-19"}]}"#;
        let found = resolve_found(answer, &pages, &[], today).unwrap();
        assert_eq!(found.len(), 1, "categories outside CATEGORIES are dropped");
        assert_eq!(found[0].url, "https://gccec.example.com/whats-on/");
    }

    #[test]
    fn fmt_dates_describes_range_and_countdown() {
        let today = d("2026-09-25");
        let mut e = expo("A", "悉尼", "2026-10-30");
        e.end = d("2026-11-01");
        assert_eq!(fmt_dates(&e, today), "10月30日（周五）–11月1日 · 还有35天");
        e.end = d("2026-10-31");
        assert_eq!(fmt_dates(&e, today), "10月30日（周五）–31日 · 还有35天");
        assert!(fmt_dates(&expo("B", "悉尼", "2026-09-26"), today).ends_with("明天开幕"));
        let mut running = expo("C", "悉尼", "2026-09-24");
        running.end = d("2026-10-04");
        assert!(fmt_dates(&running, today).ends_with("正在举办"));
    }

    #[test]
    fn render_groups_notices_and_shows_ticket_info() {
        let today = d("2026-09-25");
        let slot = Slot {
            name: "会展".into(),
            ..Slot::default()
        };
        let mut e = expo("Security Expo", "悉尼", "2026-10-02");
        e.name_zh = "安防展".into();
        e.url = "https://s.example.com".into();
        e.price = "公众 $30".into();
        e.free_ticket = "行业观众网上预登记免费".into();
        let text = render(
            &slot,
            "09-25 周五 09:00",
            &[(Notice::Week, e)],
            3,
            today,
            "",
        );
        assert!(text.contains("📅 一周内开展（1）"));
        assert!(text.contains("• [Security Expo｜安防展](https://s.example.com)"));
        assert!(text.contains("🎫 门票：公众 $30；免费：行业观众网上预登记免费"));
        assert!(!text.contains("新发现"));
        let empty = render(&slot, "t", &[], 0, today, "⚠️ x");
        assert!(empty.contains("今天没有需要通知的展会") && empty.ends_with("⚠️ x"));
    }

    #[test]
    fn database_round_trip_keeps_notice_state() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path()).unwrap();
        let mut e = expo("Security Expo", "悉尼", "2026-10-20");
        e.url = "https://s.example.com".into();
        e.id = insert_expo(&conn, &e, "now").unwrap();
        mark_notice(&mut e, Notice::Found, d("2026-09-25"), "now");
        e.enriched = true;
        e.free_ticket = "预登记免费".into();
        save_state(&conn, &e).unwrap();
        let loaded = load_upcoming(&conn, d("2026-09-25")).unwrap();
        assert_eq!(loaded, vec![e]);
        assert!(load_upcoming(&conn, d("2026-10-21")).unwrap().is_empty());
    }
}
