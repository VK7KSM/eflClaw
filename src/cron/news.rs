//! elfClaw 2026-09-24: daily news push slots as structured data, reconciled
//! into cron jobs by code.
//!
//! The chat agent manages slots (name, time, focus, sources) through the
//! `news_schedule` tool and the news worker reports per-source results
//! through `news_report`. Both go through this module, which owns
//! `HEARTBEAT_DATA.toml` end to end: load → change → validate → save → sync
//! the `news:<slot>` cron jobs. No model ever rewrites the data file itself,
//! so there is no half-written file, no lost section, and no race between the
//! worker writing ban counters and the chat agent editing a slot (every
//! change is serialized under one lock and re-reads the file from disk).
//!
//! The limits the agent cannot change — who receives the push, which
//! sub-agent runs it, quiet hours, how many slots/sources — live in a
//! `news-rules` block in `HEARTBEAT.md`, which the agent can only read.

use crate::config::Config;
use crate::cron::heartbeat_decl::ReconcileReport;
use crate::cron::{self, DeliveryConfig, JobType, Schedule};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;

pub const DATA_FILE: &str = "HEARTBEAT_DATA.toml";
pub const JOB_PREFIX: &str = "news:";
const RULES_OPEN: &str = "<!-- news-rules";
const RULES_CLOSE: &str = "-->";
const MAX_NAME_CHARS: usize = 20;
const MAX_FOCUS_CHARS: usize = 300;

const DATA_FILE_HEADER: &str = "\
# HEARTBEAT_DATA.toml — 新闻推送数据\n\
#\n\
# 本文件由程序维护：agent 通过 news_schedule（时段、新闻源）和 news_report\n\
# （候选新源）两个工具修改，不要让 agent 用 file_write 直接改。新闻由程序按\n\
# 这里的时段和新闻源抓取、过滤、去重，模型只负责挑选和写中文摘要。\n\
# 推送对象、执行的子 agent、静默时段和数量上限在 HEARTBEAT.md 的 news-rules 里。\n\
# 爸爸可以直接手改，改完下次心跳或下次工具调用时生效（格式错了会在 Telegram 报错，\n\
# 已有的新闻任务不会被删）。\n\n";

static DATA_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

fn default_ban_after() -> u32 {
    3
}

/// Limits declared in HEARTBEAT.md (agent read-only).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewsRules {
    pub agent: String,
    pub delivery: DeliveryConfig,
    pub tz: String,
    pub quiet_start: String,
    pub quiet_end: String,
    pub max_slots: usize,
    pub max_sources_per_slot: usize,
    #[serde(default = "default_ban_after")]
    pub ban_after_failures: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NewsData {
    #[serde(default, rename = "slot", skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<Slot>,
    #[serde(
        default,
        rename = "source_status",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub source_status: Vec<SourceStatus>,
    #[serde(default, rename = "dead_source", skip_serializing_if = "Vec::is_empty")]
    pub dead_sources: Vec<DeadSource>,
    #[serde(default, rename = "candidate", skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Candidate>,
}

/// elfClaw 2026-09-25: what a slot pushes. Absent in the data file = news.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SlotKind {
    /// Headlines picked from feeds / channels (`news_pipeline`).
    #[default]
    News,
    /// Upcoming expos with a 3-notice schedule (`expo_pipeline`).
    Expo,
    /// Adult-industry news plus listing intel from directory pages
    /// (`adult_pipeline`).
    Adult,
}

impl SlotKind {
    fn is_news(&self) -> bool {
        *self == SlotKind::News
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Slot {
    pub name: String,
    pub time: String,
    #[serde(default, skip_serializing_if = "SlotKind::is_news")]
    pub kind: SlotKind,
    #[serde(default)]
    pub focus: String,
    /// elfClaw 2026-09-25: prepend the code-generated market quotes block.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quotes: bool,
    /// Items per push; `None` = `news_pipeline::DEFAULT_MAX_ITEMS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_items: Option<usize>,
    #[serde(default)]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Display name in the push ("Kyiv Independent"); empty = the host name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Keep only items whose title/text contains one of these words
    /// (case-insensitive). For high-volume feeds such as market squawks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<String>,
    /// The page only has its content after JavaScript runs: fetch it with
    /// cf-crawler's browser instead of a plain HTTP request (expo slots).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub browser: bool,
    /// Adult slots: a directory / review page whose ads and reviews are
    /// extracted into the local intel database instead of pushed as news.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub directory: bool,
    /// Fetch through TinyFish (Monid) — for sites whose bot checks stop both
    /// plain HTTP and cf-crawler (expo / adult slots).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tinyfish: bool,
    /// Fetch with the machine's own Chrome (`tools/local-browser`), reusing a
    /// profile whose checks the owner cleared by hand. For pages that are
    /// built by JavaScript or that refuse every headless client.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local_browser: bool,
    /// Directory sources: regex matching the profile-page links on the page;
    /// new profiles are fetched and read as well (adult slots).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_pattern: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceStatus {
    pub url: String,
    pub failures: u32,
    pub last_failure: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub banned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeadSource {
    pub source: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    pub name: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub found: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Parse the `news-rules` block out of HEARTBEAT.md. `Ok(None)` = no block,
/// i.e. the news-slot feature is not enabled.
pub fn parse_rules(heartbeat_md: &str) -> Result<Option<NewsRules>> {
    let Some(open) = heartbeat_md.find(RULES_OPEN) else {
        return Ok(None);
    };
    let body_start = open + RULES_OPEN.len();
    let Some(close) = heartbeat_md[body_start..].find(RULES_CLOSE) else {
        bail!("news-rules 规则块缺少结尾的 '-->'");
    };
    let body = &heartbeat_md[body_start..body_start + close];
    let rules: NewsRules = toml::from_str(body).context("news-rules 规则块格式错误")?;

    if crate::config::parse_hhmm(&rules.quiet_start).is_none()
        || crate::config::parse_hhmm(&rules.quiet_end).is_none()
    {
        bail!("news-rules 的 quiet_start/quiet_end 必须是 HH:MM");
    }
    if rules.tz.parse::<chrono_tz::Tz>().is_err() {
        bail!("news-rules 的 tz '{}' 不是有效时区", rules.tz);
    }
    if rules.agent.trim().is_empty() {
        bail!("news-rules 的 agent 不能为空");
    }
    let has = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
    if !has(&rules.delivery.channel) || !has(&rules.delivery.to) {
        bail!("news-rules 的 delivery 必须写明 channel 和 to");
    }
    if rules.max_slots == 0 || rules.max_sources_per_slot == 0 || rules.ban_after_failures == 0 {
        bail!("news-rules 的 max_slots / max_sources_per_slot / ban_after_failures 必须大于 0");
    }
    Ok(Some(rules))
}

fn heartbeat_md_path(config: &Config) -> PathBuf {
    config.workspace_dir.join("HEARTBEAT.md")
}

pub fn data_path(config: &Config) -> PathBuf {
    config.workspace_dir.join(DATA_FILE)
}

/// Rules for tool calls: a missing block is an error ("feature not enabled").
pub fn load_rules(config: &Config) -> Result<NewsRules> {
    let content =
        std::fs::read_to_string(heartbeat_md_path(config)).context("读取 HEARTBEAT.md 失败")?;
    parse_rules(&content)?
        .ok_or_else(|| anyhow::anyhow!("HEARTBEAT.md 里没有 news-rules 规则块，新闻时段功能未启用"))
}

pub fn load_data(config: &Config) -> Result<NewsData> {
    let path = data_path(config);
    if !path.exists() {
        return Ok(NewsData::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("读取 {DATA_FILE} 失败"))?;
    toml::from_str(&raw).with_context(|| format!("{DATA_FILE} 格式错误"))
}

fn save_data(config: &Config, data: &NewsData) -> Result<()> {
    let body = toml::to_string_pretty(data).context("序列化新闻数据失败")?;
    let path = data_path(config);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("{DATA_FILE_HEADER}{body}"))
        .with_context(|| format!("写入 {} 失败", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("替换 {DATA_FILE} 失败"))
}

/// Load → change → validate → save, serialized against every other change.
/// Nothing is written if `change` or validation fails.
pub fn update_data<T>(
    config: &Config,
    rules: &NewsRules,
    change: impl FnOnce(&mut NewsData) -> Result<T>,
) -> Result<T> {
    let _guard = DATA_LOCK.lock();
    let mut data = load_data(config)?;
    let out = change(&mut data)?;
    validate_data(&data, rules)?;
    save_data(config, &data)?;
    Ok(out)
}

fn is_quiet(minutes: u32, rules: &NewsRules) -> bool {
    let start = crate::config::parse_hhmm(&rules.quiet_start).unwrap_or(0);
    let end = crate::config::parse_hhmm(&rules.quiet_end).unwrap_or(0);
    start != end && crate::config::is_within_active_hours(minutes, start, end)
}

pub fn validate_url(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://"))
        || url.chars().any(char::is_whitespace)
    {
        bail!("'{url}' 不是有效的 http(s) 地址");
    }
    Ok(())
}

pub fn validate_slot(slot: &Slot, rules: &NewsRules) -> Result<()> {
    let name = slot.name.trim();
    if name.is_empty() || name != slot.name {
        bail!("时段名不能为空，也不能以空格开头或结尾");
    }
    if name.chars().count() > MAX_NAME_CHARS || name.contains(['\n', ':']) {
        bail!("时段名 '{name}' 太长（最多 {MAX_NAME_CHARS} 个字）或含有换行/冒号");
    }
    let Some(minutes) = crate::config::parse_hhmm(&slot.time) else {
        bail!("时段 '{name}' 的时间 '{}' 不是 HH:MM", slot.time);
    };
    if is_quiet(minutes, rules) {
        bail!(
            "时段 '{name}' 的时间 {} 落在静默时段 {}-{} 内（HEARTBEAT.md 规定，不能推送）",
            slot.time,
            rules.quiet_start,
            rules.quiet_end
        );
    }
    if slot.focus.chars().count() > MAX_FOCUS_CHARS {
        bail!("时段 '{name}' 的关注重点太长（最多 {MAX_FOCUS_CHARS} 个字）");
    }
    if slot.sources.is_empty() {
        bail!("时段 '{name}' 至少要有一个新闻源（要停掉这个时段请删除整个时段）");
    }
    if slot.sources.len() > rules.max_sources_per_slot {
        bail!(
            "时段 '{name}' 有 {} 个新闻源，超过 HEARTBEAT.md 规定的上限 {}",
            slot.sources.len(),
            rules.max_sources_per_slot
        );
    }
    let mut seen = HashSet::new();
    for source in &slot.sources {
        validate_url(&source.url)?;
        if !seen.insert(source.url.as_str()) {
            bail!("时段 '{name}' 里重复出现了新闻源 {}", source.url);
        }
    }
    Ok(())
}

pub fn validate_data(data: &NewsData, rules: &NewsRules) -> Result<()> {
    if data.slots.len() > rules.max_slots {
        bail!(
            "新闻时段有 {} 个，超过 HEARTBEAT.md 规定的上限 {}",
            data.slots.len(),
            rules.max_slots
        );
    }
    let mut names = HashSet::new();
    for slot in &data.slots {
        validate_slot(slot, rules)?;
        if !names.insert(slot.name.as_str()) {
            bail!("时段名 '{}' 重复", slot.name);
        }
    }
    Ok(())
}

pub fn is_banned(data: &NewsData, url: &str) -> bool {
    data.source_status.iter().any(|s| s.url == url && s.banned)
}

fn slot_schedule(slot: &Slot, rules: &NewsRules) -> Schedule {
    let minutes = crate::config::parse_hhmm(&slot.time).unwrap_or(0);
    Schedule::Cron {
        expr: format!("{} {} * * *", minutes % 60, minutes / 60),
        tz: Some(rules.tz.clone()),
    }
}

/// Sources of `slot` that are not banned — what `news_pipeline` fetches.
pub fn usable_sources<'a>(slot: &'a Slot, data: &NewsData) -> Vec<&'a Source> {
    slot.sources
        .iter()
        .filter(|s| !is_banned(data, &s.url))
        .collect()
}

pub fn job_name(slot_name: &str) -> String {
    format!("{JOB_PREFIX}{slot_name}")
}

/// Sync `news:<slot>` cron jobs with HEARTBEAT_DATA.toml. Idempotent;
/// unchanged jobs are not touched. Fail-safe: when the rules or the data file
/// can't be read, nothing is created, changed or removed — a typo never
/// deletes existing news jobs.
pub fn reconcile(config: &Config) -> Result<ReconcileReport> {
    let mut report = ReconcileReport::default();

    let heartbeat = std::fs::read_to_string(heartbeat_md_path(config)).unwrap_or_default();
    let rules = match parse_rules(&heartbeat) {
        Ok(Some(rules)) => rules,
        Ok(None) => {
            if data_path(config).exists() {
                report.errors.push(format!(
                    "HEARTBEAT.md 没有 news-rules 规则块，{DATA_FILE} 里的新闻时段不会生效"
                ));
            }
            return Ok(report);
        }
        Err(e) => {
            report.errors.push(format!("{e:#}"));
            return Ok(report);
        }
    };
    if !config.agents.contains_key(&rules.agent) {
        report.errors.push(format!(
            "news-rules 指定的子 agent '{}' 没有在 config.toml 的 [agents] 里定义",
            rules.agent
        ));
        return Ok(report);
    }
    let data = {
        let _guard = DATA_LOCK.lock();
        match load_data(config) {
            Ok(data) => data,
            Err(e) => {
                report.errors.push(format!("{e:#}"));
                return Ok(report);
            }
        }
    };

    let mut declared: HashSet<String> = HashSet::new();
    let mut seen_names = HashSet::new();
    for (index, slot) in data.slots.iter().enumerate() {
        let name = job_name(&slot.name);
        declared.insert(name.clone());
        if index >= rules.max_slots {
            report.errors.push(format!(
                "时段 '{}' 超出 HEARTBEAT.md 规定的上限 {} 个，未生效",
                slot.name, rules.max_slots
            ));
            continue;
        }
        if !seen_names.insert(slot.name.as_str()) {
            report
                .errors
                .push(format!("时段名 '{}' 重复，只有第一个生效", slot.name));
            continue;
        }
        if let Err(e) = validate_slot(slot, &rules) {
            // Still declared: its existing job is kept, just not changed.
            report.errors.push(format!("{e:#}"));
            continue;
        }
        if usable_sources(slot, &data).is_empty() {
            report.errors.push(format!(
                "时段 '{}' 的新闻源都已被封禁，这个时段暂停推送",
                slot.name
            ));
            continue;
        }
        let schedule = slot_schedule(slot, &rules);

        // elfClaw 2026-09-25: news slots are code-run `JobType::News` jobs;
        // the prompt only carries the slot name (sources are read fresh from
        // the data file at fire time, so a source change needs no job update).
        let existing = cron::find_job_by_name(config, &name)?;
        if let Some(job) = &existing {
            let unchanged = job.job_type == JobType::News
                && job.schedule == schedule
                && job.prompt.as_deref() == Some(slot.name.as_str())
                && job.delivery == rules.delivery;
            if unchanged {
                continue;
            }
        }
        match cron::add_news_job(
            config,
            Some(name),
            schedule,
            &slot.name,
            Some(rules.delivery.clone()),
        ) {
            Ok(_) if existing.is_some() => report.updated.push(slot.name.clone()),
            Ok(_) => report.created.push(slot.name.clone()),
            Err(e) => report.errors.push(format!("时段 '{}': {e:#}", slot.name)),
        }
    }

    for job in cron::list_jobs(config)? {
        let Some(name) = job.name.as_deref() else {
            continue;
        };
        if name.starts_with(JOB_PREFIX) && !declared.contains(name) {
            match cron::remove_job(config, &job.id) {
                Ok(()) => report
                    .removed
                    .push(name.trim_start_matches(JOB_PREFIX).to_string()),
                Err(e) => report.errors.push(format!("删除 '{name}' 失败: {e:#}")),
            }
        }
    }
    Ok(report)
}

// ── Data operations used by the news_schedule / news_report tools ──
// Each runs inside `update_data`, so the result is validated before saving.

fn find_slot_mut<'a>(data: &'a mut NewsData, name: &str) -> Result<&'a mut Slot> {
    data.slots
        .iter_mut()
        .find(|s| s.name == name)
        .ok_or_else(|| anyhow::anyhow!("没有名为 '{name}' 的时段"))
}

/// Create or update a slot (the name is its identity). Returns true when a
/// new slot was created.
pub fn set_slot(
    data: &mut NewsData,
    name: &str,
    time: Option<&str>,
    focus: Option<&str>,
    sources: Option<Vec<Source>>,
) -> Result<bool> {
    if let Some(slot) = data.slots.iter_mut().find(|s| s.name == name) {
        if let Some(time) = time {
            slot.time = time.to_string();
        }
        if let Some(focus) = focus {
            slot.focus = focus.to_string();
        }
        if let Some(sources) = sources {
            slot.sources = sources;
        }
        return Ok(false);
    }
    let Some(time) = time else {
        bail!("新建时段 '{name}' 需要提供 time（HH:MM）");
    };
    let sources = sources.unwrap_or_default();
    if sources.is_empty() {
        bail!("新建时段 '{name}' 至少要提供一个新闻源（sources）");
    }
    data.slots.push(Slot {
        name: name.to_string(),
        time: time.to_string(),
        focus: focus.unwrap_or_default().to_string(),
        sources,
        ..Slot::default()
    });
    Ok(true)
}

pub fn remove_slot(data: &mut NewsData, name: &str) -> Result<()> {
    let before = data.slots.len();
    data.slots.retain(|s| s.name != name);
    if data.slots.len() == before {
        bail!("没有名为 '{name}' 的时段");
    }
    Ok(())
}

pub fn add_source(data: &mut NewsData, slot: &str, url: &str, note: &str) -> Result<()> {
    validate_url(url)?;
    let target = find_slot_mut(data, slot)?;
    if target.sources.iter().any(|s| s.url == url) {
        bail!("时段 '{slot}' 里已经有 {url}");
    }
    target.sources.push(Source {
        url: url.to_string(),
        note: note.to_string(),
        ..Source::default()
    });
    // A candidate that has been put to use is no longer a candidate.
    data.candidates.retain(|c| c.url != url);
    Ok(())
}

pub fn remove_source(data: &mut NewsData, slot: &str, url: &str) -> Result<()> {
    let target = find_slot_mut(data, slot)?;
    if !target.sources.iter().any(|s| s.url == url) {
        bail!("时段 '{slot}' 里没有 {url}");
    }
    if target.sources.len() == 1 {
        bail!("{url} 是时段 '{slot}' 的最后一个新闻源；要停掉这个时段请删除整个时段");
    }
    target.sources.retain(|s| s.url != url);
    Ok(())
}

/// Clear a source's failure record (e.g. the user says it works again).
pub fn unban_source(data: &mut NewsData, url: &str) -> Result<()> {
    let before = data.source_status.len();
    data.source_status.retain(|s| s.url != url);
    if data.source_status.len() == before {
        bail!("{url} 没有失败/封禁记录");
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceResult {
    pub url: String,
    pub ok: bool,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct ReportOutcome {
    pub recorded: usize,
    pub newly_banned: Vec<String>,
    pub watching: Vec<String>,
    pub ignored: Vec<String>,
    /// Failed for a transient, not-the-source's-fault reason (see
    /// `is_transient_failure`); not counted toward a ban.
    pub transient: Vec<String>,
}

/// elfClaw 2026-09-24: a failure caused by our own crawler being throttled
/// (Cloudflare Browser Rendering's per-minute browser limit — `web_scrape`
/// tells the worker to report it as "CF浏览器限流") says nothing about the
/// source, so it must not push a working source toward a ban.
fn is_transient_failure(reason: &str) -> bool {
    reason.contains("限流") || reason.to_ascii_lowercase().contains("rate limit")
}

/// Count failures in code: `ban_after_failures` consecutive-or-not failures
/// ban a source; a success clears its record. URLs that aren't a source of
/// any slot are ignored rather than recorded; transient failures are recorded
/// but not counted.
pub fn record_results(
    data: &mut NewsData,
    rules: &NewsRules,
    results: &[SourceResult],
    now: &str,
) -> ReportOutcome {
    let known: HashSet<String> = data
        .slots
        .iter()
        .flat_map(|s| s.sources.iter().map(|src| src.url.clone()))
        .collect();
    let mut outcome = ReportOutcome::default();
    for result in results {
        if !known.contains(&result.url) {
            outcome.ignored.push(result.url.clone());
            continue;
        }
        outcome.recorded += 1;
        if result.ok {
            data.source_status.retain(|s| s.url != result.url);
            continue;
        }
        if is_transient_failure(&result.reason) {
            outcome.transient.push(result.url.clone());
            continue;
        }
        let entry = match data.source_status.iter().position(|s| s.url == result.url) {
            Some(i) => &mut data.source_status[i],
            None => {
                data.source_status.push(SourceStatus {
                    url: result.url.clone(),
                    failures: 0,
                    last_failure: String::new(),
                    reason: String::new(),
                    banned: false,
                });
                data.source_status.last_mut().expect("just pushed")
            }
        };
        entry.failures += 1;
        entry.last_failure = now.to_string();
        entry.reason = result.reason.clone();
        if !entry.banned && entry.failures >= rules.ban_after_failures {
            entry.banned = true;
            outcome.newly_banned.push(result.url.clone());
        } else if !entry.banned {
            outcome.watching.push(result.url.clone());
        }
    }
    outcome
}

/// Append candidates, skipping anything already known (slot sources,
/// existing candidates, removed dead sources). Returns (added, skipped).
pub fn add_candidates(data: &mut NewsData, items: Vec<Candidate>) -> Result<(usize, usize)> {
    let (mut added, mut skipped) = (0, 0);
    for item in items {
        validate_url(&item.url)?;
        let bare = item
            .url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let known = data
            .slots
            .iter()
            .any(|s| s.sources.iter().any(|src| src.url == item.url))
            || data.candidates.iter().any(|c| c.url == item.url)
            || data.dead_sources.iter().any(|d| d.source.contains(bare));
        if known {
            skipped += 1;
        } else {
            data.candidates.push(item);
            added += 1;
        }
    }
    Ok((added, skipped))
}

/// Human/agent-readable overview for `news_schedule list`.
pub fn summary(data: &NewsData, rules: &NewsRules) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "新闻时段 {}/{} 个（静默 {}-{}，每个时段最多 {} 个源）：",
        data.slots.len(),
        rules.max_slots,
        rules.quiet_start,
        rules.quiet_end,
        rules.max_sources_per_slot
    );
    let mut slots: Vec<&Slot> = data.slots.iter().collect();
    slots.sort_by(|a, b| a.time.cmp(&b.time));
    for slot in slots {
        let _ = writeln!(
            out,
            "\n【{}】{}  重点：{}",
            slot.name, slot.time, slot.focus
        );
        for s in &slot.sources {
            let mark = if is_banned(data, &s.url) {
                "  [已封禁]"
            } else {
                ""
            };
            let _ = writeln!(out, "  - {} {}{mark}", s.url, s.note);
        }
    }
    let watching: Vec<&SourceStatus> = data.source_status.iter().collect();
    if !watching.is_empty() {
        let _ = writeln!(out, "\n失败/封禁记录：");
        for s in watching {
            let state = if s.banned { "已封禁" } else { "观察中" };
            let _ = writeln!(
                out,
                "  - {} 失败 {} 次（{}）{state}",
                s.url, s.failures, s.reason
            );
        }
    }
    if !data.candidates.is_empty() {
        let _ = writeln!(out, "\n候选新源 {} 个：", data.candidates.len());
        for c in data.candidates.iter().take(30) {
            let _ = writeln!(out, "  - [{}] [{}] {}", c.name, c.category, c.url);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const RULES: &str = r#"
# HEARTBEAT.md
<!-- news-rules
agent = "news_fetcher"
delivery = { mode = "announce", channel = "telegram", to = "zeroclaw_user" }
tz = "Australia/Sydney"
quiet_start = "23:00"
quiet_end = "06:30"
max_slots = 3
max_sources_per_slot = 3
ban_after_failures = 3
-->
"#;

    fn rules() -> NewsRules {
        parse_rules(RULES).unwrap().unwrap()
    }

    fn setup(with_agent: bool, heartbeat: &str) -> (TempDir, Config) {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        std::fs::write(config.workspace_dir.join("HEARTBEAT.md"), heartbeat).unwrap();
        if with_agent {
            let agent: crate::config::DelegateAgentConfig =
                toml::from_str(r#"allowed_tools = ["news_report"]"#).unwrap();
            config.agents.insert("news_fetcher".into(), agent);
        }
        (tmp, config)
    }

    fn slot(name: &str, time: &str, urls: &[&str]) -> Slot {
        Slot {
            name: name.into(),
            time: time.into(),
            focus: "重点".into(),
            sources: urls
                .iter()
                .map(|u| Source {
                    url: (*u).into(),
                    ..Source::default()
                })
                .collect(),
            ..Slot::default()
        }
    }

    fn src(url: &str) -> Source {
        Source {
            url: url.into(),
            ..Source::default()
        }
    }

    fn status(url: &str, banned: bool) -> SourceStatus {
        SourceStatus {
            url: url.into(),
            failures: 3,
            last_failure: String::new(),
            reason: "403".into(),
            banned,
        }
    }

    fn result(url: &str, ok: bool) -> SourceResult {
        SourceResult {
            url: url.into(),
            ok,
            reason: String::new(),
        }
    }

    fn news_jobs(config: &Config) -> Vec<crate::cron::CronJob> {
        cron::list_jobs(config)
            .unwrap()
            .into_iter()
            .filter(|j| j.name.as_deref().is_some_and(|n| n.starts_with(JOB_PREFIX)))
            .collect()
    }

    fn sydney(expr: &str) -> Schedule {
        Schedule::Cron {
            expr: expr.into(),
            tz: Some("Australia/Sydney".into()),
        }
    }

    // ── rules ──

    #[test]
    fn parses_rules_block() {
        let r = rules();
        assert_eq!(r.agent, "news_fetcher");
        assert_eq!(r.max_slots, 3);
        assert_eq!(r.delivery.to.as_deref(), Some("zeroclaw_user"));
    }

    #[test]
    fn missing_rules_block_is_none() {
        assert!(parse_rules("# nothing here").unwrap().is_none());
    }

    #[test]
    fn invalid_rules_are_rejected() {
        for (from, to) in [
            ("Australia/Sydney", "Mars/Olympus"),
            ("\"23:00\"", "\"25:00\""),
            ("to = \"zeroclaw_user\"", "to = \"\""),
            ("max_slots = 3", "max_slots = 0"),
            ("max_slots = 3", "max_slots = 3\ntypo_field = 1"),
        ] {
            let bad = RULES.replace(from, to);
            assert!(parse_rules(&bad).is_err(), "should reject: {to}");
        }
    }

    // ── slot validation ──

    #[test]
    fn slot_in_quiet_hours_is_rejected_but_edges_are_allowed() {
        let r = rules();
        let one = ["https://a.example.com"];
        assert!(validate_slot(&slot("晚", "23:30", &one), &r).is_err());
        assert!(validate_slot(&slot("早", "03:00", &one), &r).is_err());
        assert!(validate_slot(&slot("早", "06:30", &one), &r).is_ok());
        assert!(validate_slot(&slot("晚", "22:59", &one), &r).is_ok());
    }

    #[test]
    fn slot_source_rules_are_enforced() {
        let r = rules();
        let x = "https://x.example.com";
        assert!(
            validate_slot(&slot("a", "08:00", &[]), &r).is_err(),
            "no sources"
        );
        assert!(
            validate_slot(&slot("a", "08:00", &["ftp://x"]), &r).is_err(),
            "not http"
        );
        assert!(
            validate_slot(&slot("a", "08:00", &[x, x]), &r).is_err(),
            "duplicate"
        );
        let four = [
            "https://1.example.com",
            "https://2.example.com",
            "https://3.example.com",
            "https://4.example.com",
        ];
        assert!(
            validate_slot(&slot("a", "08:00", &four), &r).is_err(),
            "over max"
        );
        assert!(
            validate_slot(&slot("a:b", "08:00", &[x]), &r).is_err(),
            "colon"
        );
        assert!(validate_slot(&slot("a", "8点", &[x]), &r).is_err(), "time");
    }

    #[test]
    fn invalid_change_is_not_saved() {
        let (_tmp, config) = setup(true, RULES);
        let r = rules();
        update_data(&config, &r, |d| {
            set_slot(
                d,
                "早报",
                Some("07:00"),
                None,
                Some(vec![src("https://a.example.com")]),
            )
        })
        .unwrap();
        let before = std::fs::read_to_string(data_path(&config)).unwrap();
        let err = update_data(&config, &r, |d| {
            set_slot(d, "早报", Some("23:30"), None, None)
        });
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(data_path(&config)).unwrap(), before);
    }

    // ── reconcile ──

    #[test]
    fn reconcile_creates_one_job_per_slot_with_rules_applied() {
        let (_tmp, config) = setup(true, RULES);
        let data = NewsData {
            slots: vec![
                slot("早报", "06:30", &["https://a.example.com"]),
                slot(
                    "科技",
                    "09:30",
                    &["https://b.example.com", "https://c.example.com"],
                ),
            ],
            ..NewsData::default()
        };
        save_data(&config, &data).unwrap();
        let report = reconcile(&config).unwrap();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.created.len(), 2);

        let jobs = news_jobs(&config);
        let job = jobs
            .iter()
            .find(|j| j.name.as_deref() == Some("news:早报"))
            .unwrap();
        assert_eq!(job.job_type, JobType::News);
        assert_eq!(job.delivery.to.as_deref(), Some("zeroclaw_user"));
        assert_eq!(job.schedule, sydney("30 6 * * *"));
        assert_eq!(job.prompt.as_deref(), Some("早报"));

        let again = reconcile(&config).unwrap();
        assert_eq!(again.total_changes(), 0, "{again:?}");
    }

    #[test]
    fn removing_a_slot_removes_its_job_only() {
        let (_tmp, config) = setup(true, RULES);
        let both = NewsData {
            slots: vec![
                slot("早报", "06:30", &["https://a.example.com"]),
                slot("科技", "09:30", &["https://b.example.com"]),
            ],
            ..NewsData::default()
        };
        save_data(&config, &both).unwrap();
        reconcile(&config).unwrap();
        cron::add_message_job(
            &config,
            Some("吃药提醒".into()),
            Schedule::Cron {
                expr: "0 8 * * *".into(),
                tz: None,
            },
            "吃药",
            None,
            false,
        )
        .unwrap();

        let one = NewsData {
            slots: vec![slot("科技", "09:30", &["https://b.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &one).unwrap();
        let report = reconcile(&config).unwrap();
        assert_eq!(report.removed, vec!["早报".to_string()]);
        assert_eq!(news_jobs(&config).len(), 1);
        assert!(cron::find_job_by_name(&config, "吃药提醒")
            .unwrap()
            .is_some());
    }

    #[test]
    fn banned_sources_are_left_out_and_all_banned_pauses_the_slot() {
        let (_tmp, config) = setup(true, RULES);
        let mut data = NewsData {
            slots: vec![slot(
                "科技",
                "09:30",
                &["https://b.example.com", "https://c.example.com"],
            )],
            source_status: vec![status("https://b.example.com", true)],
            ..NewsData::default()
        };
        save_data(&config, &data).unwrap();
        reconcile(&config).unwrap();
        let usable: Vec<&str> = usable_sources(&data.slots[0], &data)
            .iter()
            .map(|s| s.url.as_str())
            .collect();
        assert_eq!(usable, vec!["https://c.example.com"]);
        assert_eq!(news_jobs(&config).len(), 1);

        data.source_status
            .push(status("https://c.example.com", true));
        save_data(&config, &data).unwrap();
        let report = reconcile(&config).unwrap();
        assert!(report.errors.iter().any(|e| e.contains("都已被封禁")));
    }

    #[test]
    fn broken_data_file_never_deletes_existing_jobs() {
        let (_tmp, config) = setup(true, RULES);
        let data = NewsData {
            slots: vec![slot("早报", "06:30", &["https://a.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &data).unwrap();
        reconcile(&config).unwrap();

        std::fs::write(data_path(&config), "[[slot]\nname = broken").unwrap();
        let report = reconcile(&config).unwrap();
        assert!(!report.errors.is_empty());
        assert!(report.removed.is_empty());
        assert_eq!(news_jobs(&config).len(), 1);
    }

    #[test]
    fn invalid_slot_keeps_its_existing_job() {
        let (_tmp, config) = setup(true, RULES);
        let good = NewsData {
            slots: vec![slot("早报", "06:30", &["https://a.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &good).unwrap();
        reconcile(&config).unwrap();
        // A hand edit moves it into quiet hours: reported, job untouched.
        let bad = NewsData {
            slots: vec![slot("早报", "23:30", &["https://a.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &bad).unwrap();
        let report = reconcile(&config).unwrap();
        assert!(!report.errors.is_empty());
        assert_eq!(news_jobs(&config)[0].schedule, sydney("30 6 * * *"));
    }

    #[test]
    fn missing_agent_changes_nothing_and_no_rules_no_data_is_silent() {
        let (_tmp, config) = setup(false, RULES);
        let data = NewsData {
            slots: vec![slot("早报", "06:30", &["https://a.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &data).unwrap();
        let report = reconcile(&config).unwrap();
        assert!(report.errors.iter().any(|e| e.contains("news_fetcher")));
        assert!(news_jobs(&config).is_empty());

        let (_tmp2, config2) = setup(true, "# no rules");
        let report = reconcile(&config2).unwrap();
        assert!(report.errors.is_empty(), "feature off: nothing to say");
    }

    // ── reports & candidates ──

    #[test]
    fn failures_are_counted_in_code_and_success_clears_them() {
        let r = rules();
        let mut data = NewsData {
            slots: vec![slot(
                "科技",
                "09:30",
                &["https://b.example.com", "https://c.example.com"],
            )],
            ..NewsData::default()
        };
        let fail_b = [result("https://b.example.com", false)];
        for _ in 0..2 {
            assert!(record_results(&mut data, &r, &fail_b, "t")
                .newly_banned
                .is_empty());
        }
        let outcome = record_results(&mut data, &r, &fail_b, "t");
        assert_eq!(
            outcome.newly_banned,
            vec!["https://b.example.com".to_string()]
        );
        assert!(is_banned(&data, "https://b.example.com"));

        record_results(
            &mut data,
            &r,
            &[result("https://c.example.com", false)],
            "t",
        );
        record_results(&mut data, &r, &[result("https://c.example.com", true)], "t");
        assert!(!data
            .source_status
            .iter()
            .any(|s| s.url == "https://c.example.com"));

        let outcome = record_results(
            &mut data,
            &r,
            &[result("https://other.example.com", false)],
            "t",
        );
        assert_eq!(outcome.ignored.len(), 1);
    }

    #[test]
    fn crawler_rate_limit_failures_do_not_count_toward_a_ban() {
        let r = rules();
        let mut data = NewsData {
            slots: vec![slot("科技", "09:30", &["https://b.example.com"])],
            ..NewsData::default()
        };
        let throttled = [SourceResult {
            url: "https://b.example.com".into(),
            ok: false,
            reason: "CF浏览器限流".into(),
        }];
        for _ in 0..5 {
            let outcome = record_results(&mut data, &r, &throttled, "t");
            assert_eq!(outcome.transient, vec!["https://b.example.com".to_string()]);
            assert!(outcome.newly_banned.is_empty() && outcome.watching.is_empty());
        }
        assert!(!is_banned(&data, "https://b.example.com"));
        assert!(data.source_status.is_empty());

        assert!(is_transient_failure(
            "Browser Rendering Rate Limit exceeded"
        ));
        assert!(!is_transient_failure("403"));
        assert!(!is_transient_failure(""));
    }

    #[test]
    fn candidates_skip_known_sources() {
        let mut data = NewsData {
            slots: vec![slot("科技", "09:30", &["https://b.example.com"])],
            dead_sources: vec![DeadSource {
                source: "SBS sbs.example.com/feed".into(),
                reason: String::new(),
                date: String::new(),
            }],
            ..NewsData::default()
        };
        let c = |url: &str| Candidate {
            name: "n".into(),
            category: String::new(),
            kind: String::new(),
            url: url.into(),
            found: String::new(),
            note: String::new(),
        };
        let (added, skipped) = add_candidates(
            &mut data,
            vec![
                c("https://b.example.com"),
                c("https://sbs.example.com/feed"),
                c("https://new.example.com"),
                c("https://new.example.com"),
            ],
        )
        .unwrap();
        assert_eq!((added, skipped), (1, 3));
    }

    #[test]
    fn concurrent_updates_are_not_lost() {
        // The chat agent and the news worker both change the file; every
        // change must survive (no read-modify-write clobbering).
        let (_tmp, config) = setup(true, RULES);
        let big = NewsRules {
            max_sources_per_slot: 50,
            ..rules()
        };
        let data = NewsData {
            slots: vec![slot("科技", "09:30", &["https://base.example.com"])],
            ..NewsData::default()
        };
        save_data(&config, &data).unwrap();
        let config = std::sync::Arc::new(config);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (config, barrier, big) = (config.clone(), barrier.clone(), big.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    update_data(&config, &big, |d| {
                        add_source(d, "科技", &format!("https://s{i}.example.com"), "")
                    })
                    .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(load_data(&config).unwrap().slots[0].sources.len(), 9);
    }
}
