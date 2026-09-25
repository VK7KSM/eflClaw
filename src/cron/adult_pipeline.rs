//! elfClaw 2026-09-25: daily adult-industry push (`kind = "adult"` slots).
//!
//! The user treats the adult industry as a legitimate, regulated industry
//! (decriminalised or licensed in several Australian states) and wants a
//! daily briefing plus a local knowledge base for industry analysis:
//!
//! - **industry news** — sources without `directory = true` go through the
//!   normal headline pipeline (`news_pipeline::run_news`) with an editor
//!   prompt framed as industry market analysis;
//! - **market intel** — `directory = true` pages (ad boards, review boards)
//!   are reduced to text; one model call turns every ad or review into a
//!   record (region, type, price, the site's verification mark, review gist,
//!   a credibility judgement). Code validates the fields, strips contact
//!   details, stores the records in `state/adult_intel.db`, computes price
//!   statistics and rewrites the local report `intel/adult-industry.md`.
//!
//! Agreed limits (memory `news_redesign_decisions.md`): nothing tries to
//! identify the real person behind an ad and no contact details are stored.
//! Ads showing signs of minors, coercion or trafficking are never recorded
//! as listings; they go to a separate `flags` table and are shown in the
//! push so they can be reported.

use crate::config::Config;
use crate::cron::expo_pipeline::{best_anchor, fetch_page, Page};
use crate::cron::news::{self, NewsRules, Slot, Source, SourceResult};
use crate::cron::news_pipeline::{
    ask_model, display_name, host_of, http_client, local_time_label, run_news,
};
use crate::security::SecurityPolicy;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// Text of one directory page handed to the model at most.
const DIRECTORY_TEXT_CHARS: usize = 12_000;
/// Statistics and the report cover records seen within this many days.
const STATS_DAYS: i64 = 30;
/// New records shown in the push at most.
const MAX_EXAMPLES: usize = 8;
/// Western listings shown per push — a comparison baseline, not the subject.
const MAX_WESTERN_EXAMPLES: usize = 3;
/// Price-statistics rows shown in the push at most.
const MAX_STAT_ROWS: usize = 6;
/// Records listed at the end of the local report.
const REPORT_RECENT: usize = 50;
const FETCH_CONCURRENCY: usize = 4;
/// New profile pages read per directory source per run.
const MAX_PROFILES: usize = 10;
/// A profile page is read again after this many days (prices change).
const PROFILE_REFRESH_DAYS: i64 = 30;
const REPORT_PATH: &str = "intel/adult-industry.md";

pub const KINDS: &[&str] = &[
    "独立",
    "经纪",
    "妓院",
    "按摩店",
    "工作室",
    "夜店KTV",
    "其他",
];
pub const SENTIMENTS: &[&str] = &["好评", "中评", "差评"];
/// Who the listing is for. The owner is Chinese and reads the Chinese/Asian
/// side as the market; Western listings are kept as a comparison baseline.
pub const ETHNICITIES: &[&str] = &["华人", "亚裔", "西人", "其他"];
pub const CREDIBILITY: &[&str] = &["可信", "存疑", "疑似虚假"];
pub const FLAGS: &[&str] = &["未成年迹象", "强迫迹象", "贩运迹象"];
/// "Names" the model writes when the material has none.
const PLACEHOLDER_NAMES: &[&str] = &["未说明", "未知", "无", "不明", "N/A", "none", "unknown"];

pub const ADULT_SELECT_SYSTEM: &str = "你是成人产业的行业分析编辑。成人服务在澳洲新州、维州、北领地已去罪化，\
昆州、首都领地等地实行牌照制度，在亚洲多地是受监管或处于灰色地带的行业。读者把它当作正规行业来研究，\
关注：法规与执法、市场与价格、经营管理、新技术新产品（sex tech、预约与安全工具、AI 伴侣、平台）、\
从业者权益与安全、亚洲各地动态、行业展会与事件。这是行业研究简报，不是色情内容：像财经编辑对待任何行业一样\
客观专业，不回避话题，也不说教。\n\
规则：\n\
1. 只能从给出的候选条目里挑选，用条目编号 id 引用；不要编造任何条目、数字或事实，不要写链接。\n\
2. 优先：法规和执法的变化、影响市场的事件、新产品新技术、有数据的行业报道、从业者安全相关的消息；\
跳过：重复报道、纯广告、名人八卦、与成人产业无关的内容；\
也跳过导购软文和 SEO 水文（「最佳/十大 XX 推荐」「如何用 VPN 访问」「XX 价格一览」这类），\
以及警方扫黄、个案判决这类社会新闻——爸爸明确说不看。\n\
3. 标题和摘要用简体中文、中性专业的措辞，不复述露骨描写；标题不超过 30 字，摘要一句话、不超过 60 字。\n\
4. category 用：法规执法、市场价格、经营管理、科技产品、从业者权益、亚洲动态、行业事件，同类用同一个名字。\n\
5. 只输出 JSON，格式：{\"items\":[{\"id\":编号,\"category\":\"分类\",\"title\":\"中文标题\",\"summary\":\"一句话摘要\"}]}，\
不要输出任何其他文字。";

const EXTRACT_SYSTEM: &str = "你是成人服务行业的市场分析员，为行业研究把广告板和评价区的公开内容整理成结构化记录。\
这是受监管行业的市场研究，不是色情内容：用中性、专业的词概括，不复述露骨描写；也不要说教或拒绝，拒绝只会让研究数据缺失。\n\
每条广告或每篇评价写一条记录，字段：\n\
- src：材料编号，例如 S0\n\
- name：广告上的艺名或店名（原文）\n\
- kind：独立、经纪、妓院、按摩店、工作室、夜店KTV、其他 之一\n\
- ethnicity：华人、亚裔、西人、其他 之一。中文广告、写明中国/台湾/香港/大陆或用中文艺名的算华人；\
日本、韩国、泰国、越南等亚洲其他地区算亚裔；欧美白人、拉丁裔算西人；看不出就填其他\n\
- region：城市或地区，用中文，例如 悉尼、墨尔本、香港、台北、东京、曼谷\n\
- price：原文价格的简述，例如「1小时 AUD 350」「HKD 800/次」，没有就填空字符串\n\
- price_hour：换算成每小时的价格数字（原币种），无法判断填 0\n\
- currency：币种代码 AUD、HKD、TWD、JPY、THB、SGD、MYR、KRW、USD 之一，没有价格就填空字符串\n\
- verified：站点是否标注已认证：是、否、未说明\n\
- review：这是给行业分析用的消费者反馈摘要，**只能**从下面这几项里挑页面实际提到的来写：\
是否守时和按约定时间、环境与卫生、沟通和服务态度、真人与照片是否相符、时长是否足量、\
是否临时加价或强推项目、是否安全（是否坚持防护措施）、性价比如何。\
**不要复述服务项目名称，不要写身体部位、外貌细节和性行为过程**——那些是广告词，对行业分析没有价值，写了等于没写。\
例如写「守时，房间干净，沟通顺畅，未加价，性价比高」，不要写具体做了什么。不超过 60 字；广告填空字符串\n\
- sentiment：评价写 好评、中评、差评；广告填空字符串\n\
- credibility：可信、存疑、疑似虚假；credibility_reason 不超过 40 字，例如：评价说真人与照片不符、\
价格远低于行情、同一内容重复刊登、要求预付定金\n\
规则：\n\
1. 只根据材料，不要编造。所有字段一律用简体中文（name 保留原文），材料是繁体或外文也要转成简体中文。\
只整理服务广告和消费者对具体店家或个人的评价；新闻报道、游记、闲聊讨论帖不写成记录，也不写进 flags；\
看不出艺名或店名的不写。\n\
2. 不要记录电话、Telegram、微信、LINE、WhatsApp、邮箱、网址等联系方式，不要推测真实身份。\n\
3. 有未成年迹象（自称学生、未满 18 岁、年龄描述明显偏小）、被强迫或被控制迹象、人口贩运迹象\
（证件被扣、背债、不能自由离开）的内容，不要写成记录，改写进 flags：{\"src\":\"S0\",\"name\":\"…\",\
\"flag\":\"未成年迹象/强迫迹象/贩运迹象 之一\",\"reason\":\"不超过 40 字\"}。「00后」这类出生年代说法本身不算未成年迹象。\n\
4. 只输出 JSON，不要其他文字：{\"listings\":[{\"src\":\"S0\",\"name\":\"…\",\"kind\":\"…\",\"ethnicity\":\"…\",\"region\":\"…\",\
\"price\":\"…\",\"price_hour\":0,\"currency\":\"…\",\"verified\":\"…\",\"review\":\"…\",\"sentiment\":\"…\",\
\"credibility\":\"…\",\"credibility_reason\":\"…\"}],\"flags\":[]}";

// ── records ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Listing {
    /// Host of the directory page the record came from.
    pub site: String,
    pub name: String,
    pub kind: String,
    pub ethnicity: String,
    pub region: String,
    pub price: String,
    pub price_hour: Option<f64>,
    pub currency: String,
    pub verified: String,
    /// Empty for an ad; the review gist for a review.
    pub review: String,
    pub sentiment: String,
    pub credibility: String,
    pub credibility_reason: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Flag {
    pub site: String,
    pub name: String,
    pub flag: String,
    pub reason: String,
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawListing {
    #[serde(default)]
    src: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    ethnicity: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    price: String,
    #[serde(default)]
    price_hour: Value,
    #[serde(default)]
    currency: String,
    #[serde(default)]
    verified: String,
    #[serde(default)]
    review: String,
    #[serde(default)]
    sentiment: String,
    #[serde(default)]
    credibility: String,
    #[serde(default)]
    credibility_reason: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawFlag {
    #[serde(default)]
    src: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    flag: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Default, Deserialize)]
struct Extracted {
    #[serde(default)]
    listings: Vec<RawListing>,
    #[serde(default)]
    flags: Vec<RawFlag>,
}

/// Remove contact details the model may have copied anyway: phone numbers
/// (8+ digits), @handles, e-mail addresses, links and "WeChat: id" forms.
pub fn strip_contacts(s: &str) -> String {
    static CONTACT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)https?://\S+|www\.\S+|[\w.+-]+@[\w-]+\.[\w.]+|@[a-z0-9_]{3,}|(?:wechat|weixin|微信|line|whatsapp|telegram|tg|kakao)\s*(?:id)?\s*[:：]?\s*[a-z0-9_.\-]{3,}",
        )
        .expect("valid regex")
    });
    static PHONE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\+?\d[\d \-().]{6,}\d").expect("valid regex")
    });
    let no_contacts = CONTACT.replace_all(s, "");
    let no_phones = PHONE.replace_all(&no_contacts, |c: &regex::Captures| {
        let digits = c[0].chars().filter(char::is_ascii_digit).count();
        if digits >= 8 {
            String::new()
        } else {
            c[0].to_string()
        }
    });
    no_phones.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean(s: &str, max: usize) -> String {
    crate::util::truncate_with_ellipsis(&strip_contacts(s.trim()), max)
}

/// Traditional → simplified for the characters that occur in the fixed
/// vocabularies and common city names. The model sometimes answers in
/// traditional Chinese when the material is (HK / TW sites); "好評" must still
/// match "好评", and "墨爾本" must group with "墨尔本" in the statistics.
pub fn to_simplified(s: &str) -> String {
    const MAP: &[(char, char)] = &[
        ('評', '评'),
        ('獨', '独'),
        ('經', '经'),
        ('紀', '纪'),
        ('虛', '虚'),
        ('強', '强'),
        ('販', '贩'),
        ('運', '运'),
        ('跡', '迹'),
        ('蹟', '迹'),
        ('齡', '龄'),
        ('說', '说'),
        ('爾', '尔'),
        ('凱', '凯'),
        ('蘭', '兰'),
        ('裡', '里'),
        ('達', '达'),
        ('灣', '湾'),
        ('臺', '台'),
        ('東', '东'),
        ('門', '门'),
        ('廣', '广'),
        ('華', '华'),
        ('亞', '亚'),
        ('國', '国'),
        ('會', '会'),
        ('館', '馆'),
        ('場', '场'),
        ('畫', '画'),
        ('線', '线'),
        ('體', '体'),
        ('證', '证'),
        ('認', '认'),
        ('實', '实'),
        ('聯', '联'),
        ('區', '区'),
        ('黃', '黄'),
        ('島', '岛'),
        ('紐', '纽'),
        ('約', '约'),
        ('羅', '罗'),
        ('馬', '马'),
        ('來', '来'),
        ('韓', '韩'),
        ('濟', '济'),
        ('龍', '龙'),
    ];
    s.chars()
        .map(|c| MAP.iter().find(|(t, _)| *t == c).map_or(c, |(_, sc)| *sc))
        .collect()
}

fn one_of(value: &str, allowed: &[&str]) -> String {
    let v = to_simplified(value.trim());
    let v = v.as_str();
    if allowed.contains(&v) {
        v.to_string()
    } else {
        String::new()
    }
}

fn page_of<'a>(pages: &'a [Page], src: &str) -> Option<&'a Page> {
    pages.get(src.trim().strip_prefix('S')?.parse::<usize>().ok()?)
}

fn json_object(answer: &str) -> Option<&str> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    answer.get(start..=end)
}

/// Model answer → validated records. Links come from the page (an anchor
/// matching the name, else the page itself), never from the model.
pub(super) fn resolve_extracted(answer: &str, pages: &[Page]) -> Option<(Vec<Listing>, Vec<Flag>)> {
    let raw: Extracted = serde_json::from_str(json_object(answer)?).ok()?;
    let link = |page: &Page, name: &str| {
        best_anchor(&page.anchors, name).unwrap_or_else(|| page.src.url.clone())
    };
    let mut listings = Vec::new();
    for r in raw.listings {
        let Some(page) = page_of(pages, &r.src) else {
            continue;
        };
        let name = clean(&r.name, 60);
        if name.is_empty() || PLACEHOLDER_NAMES.contains(&name.as_str()) {
            continue;
        }
        let currency = r.currency.trim().to_ascii_uppercase();
        let currency = if currency.len() == 3 && currency.chars().all(|c| c.is_ascii_alphabetic()) {
            currency
        } else {
            String::new()
        };
        let price_hour = r
            .price_hour
            .as_f64()
            .or_else(|| r.price_hour.as_str().and_then(|s| s.trim().parse().ok()))
            .filter(|p| *p > 0.0 && *p < 10_000_000.0 && !currency.is_empty());
        let region = to_simplified(&clean(&r.region, 20));
        let kind = one_of(&r.kind, KINDS);
        let ethnicity = one_of(&r.ethnicity, ETHNICITIES);
        let verified = one_of(&r.verified, &["是", "否"]);
        let review = clean(&r.review, 80);
        listings.push(Listing {
            site: host_of(&page.src.url),
            url: link(page, &name),
            name,
            kind: if kind.is_empty() {
                "其他".into()
            } else {
                kind
            },
            ethnicity: if ethnicity.is_empty() {
                "其他".into()
            } else {
                ethnicity
            },
            region: if region.is_empty() {
                "未知".into()
            } else {
                region
            },
            price: clean(&r.price, 60),
            price_hour,
            currency,
            verified: if verified.is_empty() {
                "未说明".into()
            } else {
                verified
            },
            sentiment: if review.is_empty() {
                String::new()
            } else {
                one_of(&r.sentiment, SENTIMENTS)
            },
            review,
            credibility: one_of(&r.credibility, CREDIBILITY),
            credibility_reason: clean(&r.credibility_reason, 60),
        });
    }
    let mut flags = Vec::new();
    for f in raw.flags {
        let Some(page) = page_of(pages, &f.src) else {
            continue;
        };
        let flag = one_of(&f.flag, FLAGS);
        let name = clean(&f.name, 60);
        if flag.is_empty() || name.is_empty() {
            continue;
        }
        flags.push(Flag {
            site: host_of(&page.src.url),
            url: link(page, &name),
            name,
            flag,
            reason: clean(&f.reason, 60),
        });
    }
    Some((listings, flags))
}

fn build_request(pages: &[Page], today: &str) -> String {
    let mut out = format!("今天是 {today}。下面是各站点的公开内容：\n");
    for (i, page) in pages.iter().enumerate() {
        let text = crate::util::truncate_with_ellipsis(&page.text, DIRECTORY_TEXT_CHARS);
        let _ = write!(out, "\n[S{i}] {}（{}）\n{text}\n", page.name, page.src.url);
    }
    out
}

// ── database ─────────────────────────────────────────────────────────────

fn open_db(workspace: &Path) -> Result<rusqlite::Connection> {
    let path = workspace.join("state").join("adult_intel.db");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let conn = rusqlite::Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS listings (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             site TEXT NOT NULL,
             name TEXT NOT NULL,
             region TEXT NOT NULL,
             review TEXT NOT NULL DEFAULT '',
             kind TEXT NOT NULL DEFAULT '',
             ethnicity TEXT NOT NULL DEFAULT '',
             price TEXT NOT NULL DEFAULT '',
             price_hour REAL,
             currency TEXT NOT NULL DEFAULT '',
             verified TEXT NOT NULL DEFAULT '',
             sentiment TEXT NOT NULL DEFAULT '',
             credibility TEXT NOT NULL DEFAULT '',
             credibility_reason TEXT NOT NULL DEFAULT '',
             url TEXT NOT NULL DEFAULT '',
             first_seen TEXT NOT NULL,
             last_seen TEXT NOT NULL,
             UNIQUE(site, name, region, review)
         );
         CREATE TABLE IF NOT EXISTS profiles_read (
             url TEXT PRIMARY KEY,
             read_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS flags (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             site TEXT NOT NULL,
             name TEXT NOT NULL,
             flag TEXT NOT NULL,
             reason TEXT NOT NULL DEFAULT '',
             url TEXT NOT NULL DEFAULT '',
             first_seen TEXT NOT NULL,
             last_seen TEXT NOT NULL,
             UNIQUE(site, name, flag)
         );",
    )?;
    Ok(conn)
}

/// Insert or refresh a record; true when it was not in the database yet.
fn upsert_listing(conn: &rusqlite::Connection, l: &Listing, stamp: &str) -> Result<bool> {
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM listings WHERE site = ?1 AND name = ?2 AND region = ?3 AND review = ?4",
            rusqlite::params![l.site, l.name, l.region, l.review],
            |r| r.get(0),
        )
        .ok();
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE listings SET kind = ?2, price = ?3, price_hour = ?4, currency = ?5,
                        verified = ?6, sentiment = ?7, credibility = ?8, credibility_reason = ?9,
                        url = ?10, last_seen = ?11, ethnicity = ?12
                 WHERE id = ?1",
                rusqlite::params![
                    id,
                    l.kind,
                    l.price,
                    l.price_hour,
                    l.currency,
                    l.verified,
                    l.sentiment,
                    l.credibility,
                    l.credibility_reason,
                    l.url,
                    stamp,
                    l.ethnicity
                ],
            )?;
            Ok(false)
        }
        None => {
            conn.execute(
                "INSERT INTO listings (site, name, region, review, kind, price, price_hour, currency,
                        verified, sentiment, credibility, credibility_reason, url, first_seen, last_seen,
                        ethnicity)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, ?15)",
                rusqlite::params![
                    l.site,
                    l.name,
                    l.region,
                    l.review,
                    l.kind,
                    l.price,
                    l.price_hour,
                    l.currency,
                    l.verified,
                    l.sentiment,
                    l.credibility,
                    l.credibility_reason,
                    l.url,
                    stamp,
                    l.ethnicity
                ],
            )?;
            Ok(true)
        }
    }
}

fn upsert_flag(conn: &rusqlite::Connection, f: &Flag, stamp: &str) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO flags (site, name, flag, reason, url, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        rusqlite::params![f.site, f.name, f.flag, f.reason, f.url, stamp],
    )?;
    if changed == 0 {
        conn.execute(
            "UPDATE flags SET last_seen = ?4 WHERE site = ?1 AND name = ?2 AND flag = ?3",
            rusqlite::params![f.site, f.name, f.flag, stamp],
        )?;
    }
    Ok(changed > 0)
}

/// A record plus the time it was first seen (report rows).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StoredListing {
    pub(super) listing: Listing,
    pub(super) first_seen: String,
}

fn recent_listings(conn: &rusqlite::Connection, since: &str) -> Result<Vec<StoredListing>> {
    let mut stmt = conn.prepare(
        "SELECT site, name, kind, region, price, price_hour, currency, verified, review, sentiment,
                credibility, credibility_reason, url, first_seen, ethnicity
         FROM listings WHERE last_seen >= ?1 ORDER BY first_seen DESC, id DESC",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok(StoredListing {
            listing: Listing {
                site: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                region: r.get(3)?,
                price: r.get(4)?,
                price_hour: r.get(5)?,
                currency: r.get(6)?,
                verified: r.get(7)?,
                review: r.get(8)?,
                sentiment: r.get(9)?,
                credibility: r.get(10)?,
                credibility_reason: r.get(11)?,
                url: r.get(12)?,
                ethnicity: r.get(14)?,
            },
            first_seen: r.get(13)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn recent_flags(conn: &rusqlite::Connection, since: &str) -> Result<Vec<(Flag, String)>> {
    let mut stmt = conn.prepare(
        "SELECT site, name, flag, reason, url, first_seen FROM flags
         WHERE last_seen >= ?1 ORDER BY first_seen DESC",
    )?;
    let rows = stmt.query_map([since], |r| {
        Ok((
            Flag {
                site: r.get(0)?,
                name: r.get(1)?,
                flag: r.get(2)?,
                reason: r.get(3)?,
                url: r.get(4)?,
            },
            r.get(5)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Profile links to read this run, per directory page: links matching the
/// source's `profile_pattern` that were not read in the last
/// `PROFILE_REFRESH_DAYS` days, at most `MAX_PROFILES` each.
fn pick_profiles(
    conn: &rusqlite::Connection,
    pages: &[Page],
    now: DateTime<Utc>,
) -> Result<Vec<(usize, Vec<String>)>> {
    let since = (now - chrono::Duration::days(PROFILE_REFRESH_DAYS)).to_rfc3339();
    let mut out = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let pattern = page.src.profile_pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        let re = match regex::Regex::new(pattern) {
            Ok(re) => re,
            Err(e) => {
                tracing::warn!(source = %page.src.url, "bad profile_pattern: {e}");
                continue;
            }
        };
        let mut urls: Vec<String> = Vec::new();
        for (_, url) in &page.anchors {
            if urls.len() == MAX_PROFILES {
                break;
            }
            if !re.is_match(url) || urls.contains(url) {
                continue;
            }
            let read = conn
                .query_row(
                    "SELECT 1 FROM profiles_read WHERE url = ?1 AND read_at >= ?2",
                    rusqlite::params![url, since],
                    |_| Ok(()),
                )
                .is_ok();
            if !read {
                urls.push(url.clone());
            }
        }
        if !urls.is_empty() {
            out.push((i, urls));
        }
    }
    Ok(out)
}

fn profile_page(parent: &Page, url: String, text: &str) -> Page {
    let name = format!("{}·资料页", parent.name);
    Page {
        src: Source {
            url,
            name: name.clone(),
            directory: true,
            ..Source::default()
        },
        name,
        text: crate::util::truncate_with_ellipsis(text, DIRECTORY_TEXT_CHARS),
        anchors: Vec::new(),
        events: Vec::new(),
    }
}

async fn fetch_profiles(
    client: &reqwest::Client,
    security: &SecurityPolicy,
    pages: &[Page],
    targets: Vec<(usize, Vec<String>)>,
) -> Vec<Page> {
    let mut out = Vec::new();
    for (i, urls) in targets {
        let parent = &pages[i];
        if parent.src.local_browser {
            match crate::tools::local_browser::fetch(security, &urls).await {
                Ok(results) => {
                    for r in results.into_iter().flatten() {
                        let text = crate::cron::expo_pipeline::page_text(&r.html, 12_000);
                        out.push(profile_page(parent, r.url, &text));
                    }
                }
                Err(e) => {
                    tracing::warn!(source = %parent.src.url, "profile fetch failed: {e:#}");
                }
            }
        } else if parent.src.tinyfish {
            match crate::cron::tinyfish::fetch(client, &urls).await {
                Ok(results) => {
                    for r in results.into_iter().flatten() {
                        out.push(profile_page(parent, r.url, &r.text));
                    }
                }
                Err(e) => {
                    tracing::warn!(source = %parent.src.url, "profile fetch failed: {e:#}");
                }
            }
        } else {
            for url in urls {
                let src = Source {
                    url: url.clone(),
                    ..Source::default()
                };
                if let Ok(page) = fetch_page(client, security, src).await {
                    out.push(profile_page(parent, url, &page.text));
                }
            }
        }
    }
    out
}

// ── statistics ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub struct PriceStat {
    pub region: String,
    pub ethnicity: String,
    pub kind: String,
    pub currency: String,
    pub count: usize,
    pub median: f64,
    pub min: f64,
    pub max: f64,
}

/// Hourly price statistics per (region, ethnicity, kind, currency), most
/// samples first. Ethnicity is part of the key so the Chinese/Asian market and
/// the Western baseline are never averaged together.
pub fn price_stats(listings: &[&Listing]) -> Vec<PriceStat> {
    let mut groups: BTreeMap<(String, String, String, String), Vec<f64>> = BTreeMap::new();
    for l in listings {
        if let Some(p) = l.price_hour {
            groups
                .entry((
                    l.region.clone(),
                    l.ethnicity.clone(),
                    l.kind.clone(),
                    l.currency.clone(),
                ))
                .or_default()
                .push(p);
        }
    }
    let mut stats: Vec<PriceStat> = groups
        .into_iter()
        .map(|((region, ethnicity, kind, currency), mut prices)| {
            prices.sort_by(f64::total_cmp);
            let n = prices.len();
            let median = if n % 2 == 1 {
                prices[n / 2]
            } else {
                (prices[n / 2 - 1] + prices[n / 2]) / 2.0
            };
            PriceStat {
                region,
                ethnicity,
                kind,
                currency,
                count: n,
                median,
                min: prices[0],
                max: prices[n - 1],
            }
        })
        .collect();
    stats.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.region.cmp(&b.region)));
    stats
}

fn money(v: f64) -> String {
    format!("{v:.0}")
}

fn md_cell(s: &str) -> String {
    s.replace('|', "/").replace('\n', " ")
}

pub(super) fn render_report(
    listings: &[StoredListing],
    flags: &[(Flag, String)],
    updated: &str,
) -> String {
    let all: Vec<&Listing> = listings.iter().map(|v| &v.listing).collect();
    let reviews = all.iter().filter(|l| !l.review.is_empty()).count();
    let mut out = format!(
        "# 成人产业市场情报\n\n由程序生成，每天推送时更新，不要手改。更新时间：{updated}\n\n\
         统计范围：最近 {STATS_DAYS} 天见到的记录 {} 条（广告 {}、评价 {reviews}），风险信号 {} 条。\n",
        all.len(),
        all.len() - reviews,
        flags.len()
    );
    out.push_str(
        "\n## 每小时价格（按地区、族裔和类型）\n\n| 地区 | 族裔 | 类型 | 币种 | 样本 | 中位数 | 最低 | 最高 |\n|---|---|---|---|---|---|---|---|\n",
    );
    for s in price_stats(&all) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            md_cell(&s.region),
            md_cell(&s.ethnicity),
            md_cell(&s.kind),
            s.currency,
            s.count,
            money(s.median),
            money(s.min),
            money(s.max)
        );
    }
    out.push_str("\n## 评价倾向（按地区）\n\n| 地区 | 好评 | 中评 | 差评 |\n|---|---|---|---|\n");
    let mut sentiment: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
    for l in &all {
        if let Some(i) = SENTIMENTS.iter().position(|s| *s == l.sentiment) {
            sentiment.entry(l.region.as_str()).or_default()[i] += 1;
        }
    }
    for (region, c) in sentiment {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            md_cell(region),
            c[0],
            c[1],
            c[2]
        );
    }
    out.push_str("\n## 真实性存疑或疑似虚假\n\n");
    for l in all
        .iter()
        .filter(|l| l.credibility == "存疑" || l.credibility == "疑似虚假")
    {
        let _ = writeln!(
            out,
            "- {}：{}（{}，{}）{} — {}",
            l.credibility, l.name, l.region, l.site, l.credibility_reason, l.url
        );
    }
    out.push_str("\n## 风险信号（未成年、强迫、贩运迹象，建议举报）\n\n");
    for (f, first_seen) in flags {
        let _ = writeln!(
            out,
            "- {}：{}（{}）{} — {} · 首次发现 {}",
            f.flag,
            f.name,
            f.site,
            f.reason,
            f.url,
            first_seen.get(..10).unwrap_or(first_seen)
        );
    }
    let _ = write!(
        out,
        "\n## 最近收录（{REPORT_RECENT} 条）\n\n| 首次发现 | 地区 | 族裔 | 类型 | 名称 | 价格 | 认证 | 评价 | 真实性 | 站点 | 链接 |\n|---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for v in listings.iter().take(REPORT_RECENT) {
        let l = &v.listing;
        let review = if l.review.is_empty() {
            String::new()
        } else {
            format!("{}：{}", l.sentiment, l.review)
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            v.first_seen.get(..10).unwrap_or(&v.first_seen),
            md_cell(&l.region),
            md_cell(&l.ethnicity),
            md_cell(&l.kind),
            md_cell(&l.name),
            md_cell(&l.price),
            l.verified,
            md_cell(&review),
            md_cell(&l.credibility),
            l.site,
            l.url
        );
    }
    out
}

// ── push section ─────────────────────────────────────────────────────────

/// Reviews first (they carry the most signal), then plain ads.
fn render_examples(out: &mut String, group: &[&Listing], cap: usize) {
    let mut examples: Vec<&Listing> = group
        .iter()
        .copied()
        .filter(|l| !l.review.is_empty())
        .collect();
    examples.extend(group.iter().copied().filter(|l| l.review.is_empty()));
    for l in examples.into_iter().take(cap) {
        let mut parts = vec![format!("{} · {}", l.region, l.kind)];
        if !l.price.is_empty() {
            parts.push(l.price.clone());
        }
        if l.verified == "是" {
            parts.push("已认证".into());
        }
        if !l.review.is_empty() {
            let label = if l.sentiment.is_empty() {
                "评价"
            } else {
                l.sentiment.as_str()
            };
            parts.push(format!("{label}：{}", l.review));
        }
        if !l.credibility.is_empty() && l.credibility != "可信" {
            parts.push(format!("真实性{}", l.credibility));
        }
        let _ = writeln!(
            out,
            "• [{}]({}) — {}",
            l.name.replace('[', "(").replace(']', ")"),
            l.url,
            parts.join(" · ")
        );
    }
}

pub fn render_market(
    new_listings: &[Listing],
    new_flags: &[Flag],
    recent: &[&Listing],
    model_ok: bool,
) -> String {
    let mut out = String::from("\n**【市场观察】**\n");
    if !model_ok {
        out.push_str("⚠️ 模型这次没有整理出广告和评价（可能被安全过滤拒绝），本地库未更新。\n");
    }
    let new_reviews = new_listings.iter().filter(|l| !l.review.is_empty()).count();
    let _ = writeln!(
        out,
        "今天新增 {} 条（广告 {}、评价 {new_reviews}），近 {STATS_DAYS} 天共 {} 条",
        new_listings.len(),
        new_listings.len() - new_reviews,
        recent.len()
    );
    for s in price_stats(recent).into_iter().take(MAX_STAT_ROWS) {
        let range = if s.count > 1 {
            format!("（{}–{}）", money(s.min), money(s.max))
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "• {} · {} · {}：{} 条，每小时中位价 {} {}{range}",
            s.region,
            s.ethnicity,
            s.kind,
            s.count,
            s.currency,
            money(s.median)
        );
    }
    let count = |c: &str| new_listings.iter().filter(|l| l.sentiment == c).count();
    if new_reviews > 0 {
        let _ = writeln!(
            out,
            "• 新增评价：好评 {} · 中评 {} · 差评 {}",
            count("好评"),
            count("中评"),
            count("差评")
        );
    }
    let fake = new_listings
        .iter()
        .filter(|l| l.credibility == "疑似虚假")
        .count();
    let doubtful = new_listings
        .iter()
        .filter(|l| l.credibility == "存疑")
        .count();
    if fake + doubtful > 0 {
        let _ = writeln!(
            out,
            "• 真实性：疑似虚假 {fake} 条、存疑 {doubtful} 条（理由见本地汇总）"
        );
    }
    // Chinese / Asian listings are the market being followed; Western ones
    // ride along as a price and service baseline, capped much lower.
    let western = |l: &&Listing| l.ethnicity == "西人";
    let asian: Vec<&Listing> = new_listings.iter().filter(|l| !western(l)).collect();
    let west: Vec<&Listing> = new_listings.iter().filter(western).collect();
    for (group, heading, cap) in [
        (asian, "新收录（华人/亚裔）：", MAX_EXAMPLES),
        (west, "西人（对比参考）：", MAX_WESTERN_EXAMPLES),
    ] {
        if group.is_empty() {
            continue;
        }
        out.push_str(heading);
        out.push('\n');
        render_examples(&mut out, &group, cap);
    }
    if !new_flags.is_empty() {
        let _ = writeln!(
            out,
            "⚠️ 风险信号：今天新发现 {} 条疑似未成年、强迫或贩运的广告（未收录为资源），可向警方或 Crime Stoppers 举报：",
            new_flags.len()
        );
        for f in new_flags {
            let _ = writeln!(
                out,
                "• {}：[{}]({}) — {}",
                f.flag,
                f.name.replace('[', "(").replace(']', ")"),
                f.url,
                f.reason
            );
        }
    }
    let _ = write!(out, "📁 本地汇总：workspace/{REPORT_PATH}");
    out
}

// ── the job ──────────────────────────────────────────────────────────────

pub async fn run(
    config: &Config,
    rules: &NewsRules,
    slot: &Slot,
    sources: Vec<Source>,
) -> Result<String> {
    let (directories, feeds): (Vec<Source>, Vec<Source>) =
        sources.into_iter().partition(|s| s.directory);
    let now = Utc::now();
    let mut out = if feeds.is_empty() {
        format!("📰 **{}** | {}\n", slot.name, local_time_label(rules, now))
    } else {
        match run_news(config, rules, slot, feeds, ADULT_SELECT_SYSTEM).await {
            Ok(text) => text,
            Err(e) => format!(
                "📰 **{}** | {}\n⚠️ 行业动态这次没有生成：{e:#}\n",
                slot.name,
                local_time_label(rules, now)
            ),
        }
    };
    if !directories.is_empty() {
        match Box::pin(run_market(config, rules, slot, directories, now)).await {
            Ok(section) => out.push_str(&section),
            Err(e) => {
                let _ = write!(out, "\n⚠️ 市场观察这次没有生成：{e:#}");
            }
        }
    }
    Ok(out)
}

async fn run_market(
    config: &Config,
    rules: &NewsRules,
    slot: &Slot,
    directories: Vec<Source>,
    now: DateTime<Utc>,
) -> Result<String> {
    let stamp = now.to_rfc3339();
    let client = http_client()?;
    let security = std::sync::Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
    let tasks: Vec<_> = directories
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
                tracing::warn!(slot = %slot.name, source = %url, "directory source failed: {reason}");
                failed.push(label);
                results.push(SourceResult {
                    url,
                    ok: false,
                    reason,
                });
            }
        }
    }

    // Profile pages linked from the directory pages (the prices live there).
    let targets = pick_profiles(&open_db(&config.workspace_dir)?, &pages, now)?;
    let profiles = fetch_profiles(&client, &security, &pages, targets).await;
    let profile_urls: Vec<String> = profiles.iter().map(|p| p.src.url.clone()).collect();
    pages.extend(profiles);

    let mut model_ok = true;
    let (listings, flags) = if pages.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let request = build_request(&pages, &now.format("%Y-%m-%d").to_string());
        match ask_model(config, EXTRACT_SYSTEM, &request).await {
            Ok(answer) => resolve_extracted(&answer, &pages).unwrap_or_else(|| {
                tracing::warn!(slot = %slot.name, "adult intel model answer unusable");
                model_ok = false;
                (Vec::new(), Vec::new())
            }),
            Err(e) => {
                tracing::warn!(slot = %slot.name, "adult intel model call failed: {e:#}");
                model_ok = false;
                (Vec::new(), Vec::new())
            }
        }
    };

    let conn = open_db(&config.workspace_dir)?;
    if model_ok {
        for url in &profile_urls {
            conn.execute(
                "INSERT OR REPLACE INTO profiles_read (url, read_at) VALUES (?1, ?2)",
                rusqlite::params![url, stamp],
            )?;
        }
    }
    let mut new_listings = Vec::new();
    for l in listings {
        if upsert_listing(&conn, &l, &stamp)? {
            new_listings.push(l);
        }
    }
    let mut new_flags = Vec::new();
    for f in flags {
        if upsert_flag(&conn, &f, &stamp)? {
            new_flags.push(f);
        }
    }
    let since = (now - chrono::Duration::days(STATS_DAYS)).to_rfc3339();
    let recent = recent_listings(&conn, &since)?;
    let report = render_report(
        &recent,
        &recent_flags(&conn, &since)?,
        &local_time_label(rules, now),
    );
    let report_path = config.workspace_dir.join(REPORT_PATH);
    if let Some(dir) = report_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&report_path, report)?;

    let outcome = news::update_data(config, rules, |d| {
        Ok(news::record_results(d, rules, &results, &stamp))
    });
    let recent_refs: Vec<&Listing> = recent.iter().map(|v| &v.listing).collect();
    let mut section = render_market(&new_listings, &new_flags, &recent_refs, model_ok);
    if !failed.is_empty() {
        let _ = write!(section, "\n⚠️ 情报源这次没抓到：{}", failed.join("、"));
    }
    match outcome {
        Ok(o) if !o.newly_banned.is_empty() => {
            if let Err(e) = news::reconcile(config) {
                tracing::warn!("news reconcile after ban failed: {e:#}");
            }
            let _ = write!(section, "\n⚠️ 新封禁：{}", o.newly_banned.join("、"));
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("recording adult source results failed: {e:#}"),
    }
    Ok(section)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str, anchors: Vec<(String, String)>) -> Page {
        Page {
            src: Source {
                url: url.into(),
                directory: true,
                ..Source::default()
            },
            name: "Board".into(),
            text: "x".repeat(300),
            anchors,
            events: Vec::new(),
        }
    }

    fn listing(region: &str, kind: &str, currency: &str, price: Option<f64>) -> Listing {
        Listing {
            site: "board.example.com".into(),
            name: format!("{region}-{kind}-{price:?}"),
            kind: kind.into(),
            ethnicity: "华人".into(),
            region: region.into(),
            currency: currency.into(),
            price_hour: price,
            ..Listing::default()
        }
    }

    #[test]
    fn strip_contacts_removes_contact_details_but_keeps_prices() {
        assert_eq!(
            strip_contacts("TG預約@Beautygirl888 電話 +61 480 802 971 HKD 800/次"),
            "TG預約 電話 HKD 800/次"
        );
        assert_eq!(
            strip_contacts(
                "WeChat: lily_2026 email a.b@example.com https://x.example.com/p 1小时 AUD 350"
            ),
            "email 1小时 AUD 350"
        );
        assert_eq!(
            strip_contacts("45分鐘 1500 / 90分鐘 2800"),
            "45分鐘 1500 / 90分鐘 2800"
        );
    }

    #[test]
    fn resolve_extracted_validates_fields_and_takes_links_from_the_page() {
        let pages = vec![page(
            "https://board.example.com/zh/",
            vec![(
                "台灣靚模Lily 詳情".into(),
                "https://board.example.com/zh/ad/1".into(),
            )],
        )];
        let answer = r#"```json
{"listings":[
 {"src":"S0","name":"台灣靚模Lily","kind":"工作室","ethnicity":"華人","region":"香港","price":"HKD 800/次 微信: lily888",
  "price_hour":"800","currency":"hkd","verified":"是","review":"","sentiment":"好评",
  "credibility":"存疑","credibility_reason":"价格远低于行情","url":"https://evil.example.com"},
 {"src":"S0","name":"小淇","kind":"未知类型","region":"","price":"","price_hour":500,"currency":"",
  "verified":"maybe","review":"环境干净，真人与照片相符","sentiment":"好评","credibility":"可信"},
 {"src":"S0","name":"未说明","kind":"按摩店","region":"台北"},
 {"src":"S3","name":"bad source"},
 {"src":"S0","name":"  "}
],
"flags":[
 {"src":"S0","name":"學生妹","flag":"未成年迹象","reason":"自称在读中学生"},
 {"src":"S0","name":"x","flag":"其他","reason":"?"}
]}
```"#;
        let (listings, flags) = resolve_extracted(answer, &pages).unwrap();
        assert_eq!(listings.len(), 2);
        let a = &listings[0];
        assert_eq!(a.site, "board.example.com");
        assert_eq!(a.url, "https://board.example.com/zh/ad/1");
        assert_eq!(a.price, "HKD 800/次");
        assert_eq!((a.price_hour, a.currency.as_str()), (Some(800.0), "HKD"));
        assert_eq!(a.verified, "是");
        assert_eq!(a.ethnicity, "华人", "traditional 華人 normalises");
        assert_eq!(a.sentiment, "", "an ad has no sentiment");
        assert_eq!(a.credibility, "存疑");
        let b = &listings[1];
        assert_eq!(b.kind, "其他");
        assert_eq!(b.ethnicity, "其他", "missing ethnicity falls back");
        assert_eq!(b.region, "未知");
        assert_eq!(
            b.price_hour, None,
            "a price without currency is not a statistic"
        );
        assert_eq!(b.verified, "未说明");
        assert_eq!(b.sentiment, "好评");
        assert_eq!(b.url, "https://board.example.com/zh/");
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].flag, "未成年迹象");
        assert!(resolve_extracted("I can't help with that.", &pages).is_none());
    }

    #[test]
    fn traditional_answers_still_match_the_vocabularies() {
        assert_eq!(one_of(" 好評 ", SENTIMENTS), "好评");
        assert_eq!(one_of("獨立", KINDS), "独立");
        assert_eq!(one_of("疑似虛假", CREDIBILITY), "疑似虚假");
        assert_eq!(one_of("販運迹象", FLAGS), "贩运迹象");
        assert_eq!(
            to_simplified("墨爾本、凱恩斯、臺北"),
            "墨尔本、凯恩斯、台北"
        );
    }

    #[test]
    fn price_stats_groups_by_region_kind_and_currency() {
        let rows = [
            listing("悉尼", "独立", "AUD", Some(300.0)),
            listing("悉尼", "独立", "AUD", Some(500.0)),
            listing("悉尼", "独立", "AUD", Some(350.0)),
            listing("悉尼", "独立", "AUD", None),
            listing("香港", "工作室", "HKD", Some(800.0)),
            listing("香港", "工作室", "HKD", Some(600.0)),
        ];
        let refs: Vec<&Listing> = rows.iter().collect();
        let stats = price_stats(&refs);
        assert_eq!(stats.len(), 2);
        assert_eq!(
            (
                stats[0].region.as_str(),
                stats[0].ethnicity.as_str(),
                stats[0].count,
                stats[0].median,
                stats[0].min,
                stats[0].max
            ),
            ("悉尼", "华人", 3, 350.0, 300.0, 500.0)
        );
        assert_eq!((stats[1].count, stats[1].median), (2, 700.0));
    }

    #[test]
    fn database_upserts_report_new_records_once() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_db(dir.path()).unwrap();
        let mut l = listing("悉尼", "独立", "AUD", Some(300.0));
        assert!(upsert_listing(&conn, &l, "2026-09-25T00:00:00+00:00").unwrap());
        l.price_hour = Some(320.0);
        assert!(!upsert_listing(&conn, &l, "2026-09-26T00:00:00+00:00").unwrap());
        let mut review = l.clone();
        review.review = "准时，环境好".into();
        assert!(upsert_listing(&conn, &review, "2026-09-26T00:00:00+00:00").unwrap());
        let stored = recent_listings(&conn, "2026-09-20").unwrap();
        assert_eq!(stored.len(), 2);
        assert!(
            stored
                .iter()
                .any(|s| s.listing.price_hour == Some(320.0)
                    && s.first_seen.starts_with("2026-09-25"))
        );

        let f = Flag {
            site: "board.example.com".into(),
            name: "x".into(),
            flag: "强迫迹象".into(),
            reason: "r".into(),
            url: "u".into(),
        };
        assert!(upsert_flag(&conn, &f, "2026-09-25").unwrap());
        assert!(!upsert_flag(&conn, &f, "2026-09-26").unwrap());
        assert_eq!(recent_flags(&conn, "2026-09-26").unwrap().len(), 1);
    }

    #[test]
    fn render_market_shows_stats_examples_and_flags() {
        let mut review = listing("香港", "工作室", "HKD", Some(800.0));
        review.name = "小淇".into();
        review.url = "https://board.example.com/r/1".into();
        review.review = "真人与照片相符".into();
        review.sentiment = "好评".into();
        let mut fake = listing("香港", "工作室", "HKD", Some(600.0));
        fake.credibility = "疑似虚假".into();
        let new = vec![review.clone(), fake.clone()];
        let flags = vec![Flag {
            site: "board.example.com".into(),
            name: "學生妹".into(),
            flag: "未成年迹象".into(),
            reason: "自称中学生".into(),
            url: "https://board.example.com/a/9".into(),
        }];
        let recent: Vec<&Listing> = new.iter().collect();
        let text = render_market(&new, &flags, &recent, true);
        assert!(text.contains("今天新增 2 条（广告 1、评价 1）"));
        assert!(text.contains("• 香港 · 华人 · 工作室：2 条，每小时中位价 HKD 700（600–800）"));
        assert!(text.contains("好评 1 · 中评 0 · 差评 0"));
        assert!(text.contains("疑似虚假 1 条"));
        assert!(text.contains("新收录（华人/亚裔）："));
        assert!(text.contains(
            "• [小淇](https://board.example.com/r/1) — 香港 · 工作室 · 好评：真人与照片相符"
        ));
        let mut west = listing("悉尼", "独立", "AUD", Some(900.0));
        west.name = "Lola".into();
        west.ethnicity = "西人".into();
        let mixed = vec![review, west];
        let refs: Vec<&Listing> = mixed.iter().collect();
        let both = render_market(&mixed, &[], &refs, true);
        let asian_at = both.find("新收录（华人/亚裔）：").unwrap();
        let west_at = both.find("西人（对比参考）：").unwrap();
        assert!(asian_at < west_at, "Chinese/Asian listings come first");
        assert!(both.contains("• [Lola]"));
        assert!(text.contains("• 未成年迹象：[學生妹](https://board.example.com/a/9) — 自称中学生"));
        assert!(!text.contains("模型这次没有整理出"));
        assert!(render_market(&[], &[], &[], false).contains("模型这次没有整理出"));
    }

    #[test]
    fn render_report_writes_price_table_and_flags() {
        let rows = vec![StoredListing {
            listing: Listing {
                credibility: "存疑".into(),
                credibility_reason: "重复刊登".into(),
                price: "1小时 AUD 350 | 2小时 600".into(),
                ..listing("悉尼", "独立", "AUD", Some(350.0))
            },
            first_seen: "2026-09-25T04:00:00+00:00".into(),
        }];
        let flags = vec![(
            Flag {
                site: "s".into(),
                name: "n".into(),
                flag: "贩运迹象".into(),
                reason: "证件被扣".into(),
                url: "u".into(),
            },
            "2026-09-25T04:00:00+00:00".into(),
        )];
        let md = render_report(&rows, &flags, "09-25 14:00");
        assert!(md.contains("| 悉尼 | 华人 | 独立 | AUD | 1 | 350 | 350 | 350 |"));
        assert!(md.contains("- 存疑：悉尼-独立-Some(350.0)（悉尼，board.example.com）重复刊登"));
        assert!(md.contains("- 贩运迹象：n（s）证件被扣 — u · 首次发现 2026-09-25"));
        assert!(
            md.contains("1小时 AUD 350 / 2小时 600"),
            "pipes are escaped in table cells"
        );
    }
}
