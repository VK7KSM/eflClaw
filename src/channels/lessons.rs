//! elfClaw 2026-10-20: turning corrections into lessons (see `memory::lessons`).
//!
//! When a Telegram message reads like a correction ("不对", "你漏了",
//! "实际上"…), the turn that answers it is handed to a background review:
//! one model call reads the conversation — including which tools were
//! called and what they returned — and either proposes one general
//! procedure ("when …, do …") or says there is nothing to learn. A proposal
//! goes to the owner as a card with "✅ 记住 / ❌ 不用"; only "记住" makes it
//! active, and active lessons are what the prompt carries. `/reflect` asks
//! for the same review on demand, and the nightly chat summary is a
//! fallback for corrections the wording check missed.
//!
//! Code decides when to review, what the model sees, what it may propose
//! (the filter below) and what becomes active (the owner); the model only
//! writes the text of the proposal.

use crate::cctv::tg::{row, Bot, Buttons, SendOpts};
use crate::channels::telegram::{CallbackAnswer, CallbackPress};
use crate::memory::lessons::{Lesson, LessonStore, Status};
use crate::providers::{ChatMessage, Provider};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

const PREFIX: &str = "lesson:";
/// Messages of the conversation the review reads, at most.
const TAIL: usize = 16;
const MAX_EXCERPT_CHARS: usize = 12_000;

/// Wording that usually means the user is correcting the assistant. A miss
/// costs nothing (the nightly summary is the fallback); a false hit costs
/// one cheap review that answers "nothing to learn".
const CORRECTION_HINTS: &[&str] = &[
    "不对",
    "错了",
    "搞错",
    "弄错",
    "看错",
    "说错",
    "记错",
    "算错",
    "不是这样",
    "不准",
    "不正确",
    "你没",
    "你漏",
    "漏了",
    "没看",
    "没查",
    "应该是",
    "应该先",
    "应该去",
    "实际上",
    "其实",
    "纠正",
    "瞎说",
    "乱说",
    "瞎猜",
    "胡说",
    "wrong",
    "incorrect",
    "not right",
];

/// Proposals that touch these are dropped before the owner sees them:
/// lessons may change how elfClaw answers, never what it is allowed to do.
const FORBIDDEN: &[&str] = &[
    "权限",
    "授权",
    "审批",
    "auto_approve",
    "always_ask",
    "安全策略",
    "estop",
    "密码",
    "密钥",
    "token",
    "删除数据",
    "关闭布防",
    "撤防",
];

pub fn looks_like_correction(text: &str) -> bool {
    let lower = text.to_lowercase();
    CORRECTION_HINTS.iter().any(|h| lower.contains(h))
}

struct Review {
    bot: Bot,
    /// Owner's Telegram user name, without `@`.
    owner: String,
    workspace: PathBuf,
    provider: Arc<dyn Provider>,
    model: String,
}

static REVIEW: OnceLock<Review> = OnceLock::new();

fn store() -> Option<LessonStore> {
    let r = REVIEW.get()?;
    LessonStore::open(&r.workspace)
        .map_err(|e| tracing::warn!("lessons: cannot open store: {e:#}"))
        .ok()
}

fn is_owner(owner: &str, user: &str) -> bool {
    !owner.is_empty() && owner.eq_ignore_ascii_case(user.trim_start_matches('@'))
}

/// Set up reviews, the card buttons and `/lessons`. Needs Telegram and an
/// owner (`[chat_log] owner`); without either, nothing is learned and the
/// prompt simply carries no lessons.
pub fn register(config: &crate::config::Config) {
    let Some(token) = config
        .channels_config
        .telegram
        .as_ref()
        .map(|t| t.bot_token.clone())
        .filter(|t| !t.is_empty())
    else {
        return;
    };
    let Some(owner) = config
        .chat_log
        .owner
        .clone()
        .map(|o| o.trim_start_matches('@').to_string())
        .filter(|o| !o.is_empty())
    else {
        tracing::info!("lessons: no [chat_log] owner, corrections are not reviewed");
        return;
    };
    let options = crate::providers::ProviderRuntimeOptions {
        zeroclaw_dir: config.config_path.parent().map(PathBuf::from),
        secrets_encrypt: config.secrets.encrypt,
        ..Default::default()
    };
    let provider = match crate::providers::create_resilient_provider_with_options(
        config.default_provider.as_deref().unwrap_or("gemini"),
        config.api_key.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &options,
    ) {
        Ok(p) => Arc::from(p),
        Err(e) => {
            tracing::warn!("lessons: no provider for reviews: {e:#}");
            return;
        }
    };
    let model = config
        .worker_model
        .clone()
        .or_else(|| config.default_model.clone())
        .unwrap_or_default();
    let _ = REVIEW.set(Review {
        bot: Bot::new(&token),
        owner,
        workspace: config.workspace_dir.clone(),
        provider,
        model,
    });
    crate::channels::telegram::register_callback_handler(PREFIX, Arc::new(on_press));
    crate::channels::telegram::register_menu_command(
        "lessons",
        "经验手册：看已记住的经验",
        Arc::new(|chat: &str, sender: &str| {
            let Some(r) = REVIEW.get() else {
                return "经验手册没有开启。".into();
            };
            if !is_owner(&r.owner, sender) {
                return "只有主人能看经验手册。".into();
            }
            let chat = chat.to_string();
            tokio::spawn(async move { show_lessons(&chat).await });
            String::new()
        }),
    );
}

// ── review ───────────────────────────────────────────────────────────────

/// The user's text of a history message: the memory block and timestamp
/// the channel adds in front are not what they said.
fn user_text(content: &str) -> &str {
    let body = if content.starts_with("[Memory context]") {
        content.split_once("\n\n").map_or(content, |(_, rest)| rest)
    } else {
        content
    };
    body.trim()
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// The end of a conversation as plain lines, tool calls and results
/// included, for the review.
fn excerpt(history: &[ChatMessage]) -> String {
    let skip = history
        .iter()
        .filter(|m| m.role != "system")
        .count()
        .saturating_sub(TAIL);
    let mut out = String::new();
    for m in history.iter().filter(|m| m.role != "system").skip(skip) {
        let line = match m.role.as_str() {
            "user" => format!("用户：{}", cut(user_text(&m.content), 800)),
            "assistant" => match serde_json::from_str::<serde_json::Value>(&m.content) {
                Ok(v) if v["tool_calls"].is_array() => {
                    let calls: Vec<String> = v["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            format!(
                                "{}({})",
                                c["name"].as_str().unwrap_or("?"),
                                cut(c["arguments"].as_str().unwrap_or(""), 200)
                            )
                        })
                        .collect();
                    format!("助手调用工具：{}", calls.join("；"))
                }
                _ => format!("助手：{}", cut(&m.content, 800)),
            },
            "tool" => {
                let body = serde_json::from_str::<serde_json::Value>(&m.content)
                    .ok()
                    .and_then(|v| v["content"].as_str().map(str::to_string))
                    .unwrap_or_else(|| m.content.clone());
                format!("工具返回：{}", cut(&body, 600))
            }
            _ => continue,
        };
        out.push_str(&line);
        out.push('\n');
    }
    if out.chars().count() > MAX_EXCERPT_CHARS {
        let n = out.chars().count() - MAX_EXCERPT_CHARS;
        out = out.chars().skip(n).collect();
    }
    out
}

fn review_prompt(conversation: &str, note: Option<&str>, known: &[Lesson]) -> String {
    let mut p = String::from(
        "你在帮家庭助手 elfClaw 从用户的纠正里总结经验。下面是最近的对话，包括助手调用了哪些工具、工具返回了什么。\n\n\
         如果用户是在纠正助手（做法不对、查漏了、结论或时间错了、猜测不合理等），请总结**一条**下次遇到同类情况该怎么做的通用做法。要求：\n\
         - 写成做法，不是事后描述；要能用在同一类问题上，不要只针对这一次的具体物品、人或时间；\n\
         - 具体到该先查什么、用哪个工具或哪类数据、怎么核对，不要写「更仔细」这类空话；\n\
         - 不涉及权限、授权、安全设置、删除数据；\n\
         - 家里的事实（谁是谁、东西平时放哪）不是经验，不要写成经验。\n\
         如果用户不是在纠正助手，或者总结不出可用的做法，只输出 {\"lesson\": null}。\n\
         否则只输出一个 JSON，不要别的文字：\n\
         {\"applies_when\": \"什么情况下适用，一句话，例如「用户问某件事是几点发生的」\", \"rule\": \"该怎么做，一到两句\", \"evidence\": \"这次哪里错了、用户怎么纠正的，一句话\"}\n",
    );
    if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
        let _ = write!(p, "\n用户特别说明：{}\n", note.trim());
    }
    if !known.is_empty() {
        p.push_str("\n已有的经验（不要重复）：\n");
        for l in known {
            let _ = writeln!(p, "- 当{}：{}", l.applies_when, l.rule);
        }
    }
    let _ = write!(p, "\n对话：\n{conversation}");
    p
}

#[derive(Debug, PartialEq)]
struct Proposal {
    applies_when: String,
    rule: String,
    evidence: String,
}

/// The review's answer, if it proposes something usable and allowed.
fn parse_proposal(answer: &str) -> Option<Proposal> {
    let start = answer.find('{')?;
    let end = answer.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(answer.get(start..=end)?).ok()?;
    let field = |k: &str, max: usize| {
        v[k].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().count() <= max)
            .map(str::to_string)
    };
    let p = Proposal {
        applies_when: field("applies_when", 200)?,
        rule: field("rule", 400)?,
        evidence: field("evidence", 300).unwrap_or_default(),
    };
    let all = format!("{} {}", p.applies_when, p.rule).to_lowercase();
    if FORBIDDEN.iter().any(|w| all.contains(&w.to_lowercase())) {
        tracing::info!("lessons: proposal touches a forbidden topic, dropped");
        return None;
    }
    Some(p)
}

/// Review a conversation in the background and, if it yields a lesson,
/// send the owner a card. `source` is the user who corrected.
pub fn spawn_review(source: String, conversation: String, note: Option<String>) {
    if REVIEW.get().is_none() || conversation.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = review(&source, &conversation, note.as_deref()).await {
            tracing::warn!("lessons: review failed: {e:#}");
        }
    });
}

/// The nightly fallback: review unless `source` already had a lesson
/// proposed since `since` (the correction was caught at the time).
pub fn spawn_review_unless_proposed(
    source: String,
    since: chrono::DateTime<chrono::Utc>,
    conversation: String,
    note: String,
) {
    let Some(store) = store() else {
        return;
    };
    if store.proposed_since(&source, since).unwrap_or(true) {
        return;
    }
    spawn_review(source, conversation, Some(note));
}

/// `spawn_review` over a channel history (tool calls included).
pub fn spawn_review_of_history(source: &str, history: &[ChatMessage], note: Option<String>) {
    spawn_review(source.to_string(), excerpt(history), note);
}

async fn review(source: &str, conversation: &str, note: Option<&str>) -> anyhow::Result<()> {
    let r = REVIEW.get().ok_or_else(|| anyhow::anyhow!("not set up"))?;
    let store = LessonStore::open(&r.workspace)?;
    let mut known = store.with_status(Status::Active, 50)?;
    known.extend(store.with_status(Status::Proposed, 20)?);
    let prompt = review_prompt(conversation, note, &known);
    let answer = r.provider.simple_chat(&prompt, &r.model, 0.2).await?;
    let Some(p) = parse_proposal(&answer) else {
        tracing::info!(source, "lessons: nothing to learn from this correction");
        return Ok(());
    };
    if known.iter().any(|l| l.rule == p.rule) {
        return Ok(());
    }
    let lesson = store.propose(&p.applies_when, &p.rule, &p.evidence, source)?;
    crate::elfclaw_log::log(crate::elfclaw_log::LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: crate::elfclaw_log::LogLevel::Info,
        category: crate::elfclaw_log::LogCategory::AgentLifecycle,
        component: "lessons".into(),
        message: "Lesson proposed".into(),
        details: serde_json::json!({"id": lesson.id, "source": source}),
    });
    send_card(r, &lesson).await
}

// ── the owner's card ─────────────────────────────────────────────────────

fn card_text(l: &Lesson) -> String {
    let mut t = format!(
        "📝 想记住一条经验（经验 #{}，来自 {} 的纠正）\n适用：{}\n做法：{}",
        l.id, l.source, l.applies_when, l.rule
    );
    if !l.evidence.is_empty() {
        let _ = write!(t, "\n依据：{}", l.evidence);
    }
    match l.status {
        Status::Proposed => {}
        Status::Active => t.push_str("\n\n✅ 已记住，以后遇到这种情况会照做"),
        Status::Rejected => t.push_str("\n\n❌ 已丢弃"),
        Status::Retired => t.push_str("\n\n🗑 已撤销，不再使用"),
    }
    t
}

fn card_buttons(l: &Lesson) -> Buttons {
    let ok = format!("{PREFIX}ok:{}", l.id);
    let no = format!("{PREFIX}no:{}", l.id);
    let rm = format!("{PREFIX}rm:{}", l.id);
    match l.status {
        Status::Proposed => row(&[("✅ 记住", &ok), ("❌ 不用", &no)]),
        Status::Active => row(&[("🗑 撤销这条", &rm)]),
        Status::Rejected | Status::Retired => Vec::new(),
    }
}

/// The owner's private chat: their chat-log files name it.
fn owner_chat(r: &Review) -> Option<String> {
    let mut files: Vec<_> = crate::channels::chat_log::list_log_files(&r.workspace)
        .ok()?
        .into_iter()
        .filter(|f| is_owner(&r.owner, &f.username))
        .collect();
    files.sort_by(|a, b| b.date.cmp(&a.date));
    let text = std::fs::read_to_string(&files.first()?.path).ok()?;
    let log: crate::channels::chat_log::DailyChatLog = serde_json::from_str(&text).ok()?;
    Some(log.chat_id).filter(|c| !c.is_empty())
}

async fn send_card(r: &Review, l: &Lesson) -> anyhow::Result<()> {
    let chat = owner_chat(r).ok_or_else(|| anyhow::anyhow!("owner's chat not known yet"))?;
    let opts = SendOpts {
        silent: false,
        parent: None,
        buttons: card_buttons(l),
    };
    r.bot.send_text(&chat, &card_text(l), &opts).await?;
    Ok(())
}

fn on_press(press: &CallbackPress<'_>) -> Option<CallbackAnswer> {
    let r = REVIEW.get()?;
    let answer = |toast: &str| {
        Some(CallbackAnswer {
            toast: toast.to_string(),
            clear_buttons: false,
        })
    };
    if !is_owner(&r.owner, press.user) {
        return answer("只有主人能确认经验");
    }
    let rest = press.data.strip_prefix(PREFIX)?;
    let (verb, id) = rest.split_once(':')?;
    let id: i64 = id.parse().ok()?;
    let to = match verb {
        "ok" => Status::Active,
        "no" => Status::Rejected,
        "rm" => Status::Retired,
        _ => return None,
    };
    let store = store()?;
    let lesson = match store.decide(id, to) {
        Ok(Some(l)) => l,
        Ok(None) => match store.get(id) {
            Ok(Some(l)) => l,
            _ => return answer("这条经验已经不在了"),
        },
        Err(e) => {
            tracing::warn!("lessons: decide failed: {e:#}");
            return answer("没改成，请再按一次");
        }
    };
    if let (Some(chat), Some(message)) = (press.chat_id, press.message_id) {
        let (text, buttons) = (card_text(&lesson), card_buttons(&lesson));
        tokio::spawn(async move {
            let chat = chat.to_string();
            if let Err(e) = r.bot.edit(&chat, message, &text, false, &buttons).await {
                tracing::debug!("lessons: card edit failed: {e:#}");
            }
        });
    }
    answer(match lesson.status {
        Status::Active => "已记住",
        Status::Rejected => "已丢弃",
        Status::Retired => "已撤销",
        Status::Proposed => "",
    })
}

async fn show_lessons(chat: &str) {
    let (Some(r), Some(store)) = (REVIEW.get(), store()) else {
        return;
    };
    let active = store.with_status(Status::Active, 50).unwrap_or_default();
    let pending = store.with_status(Status::Proposed, 20).unwrap_or_default();
    let opts = SendOpts::default();
    if active.is_empty() && pending.is_empty() {
        let _ = r.bot.send_text(chat, "经验手册还是空的。", &opts).await;
        return;
    }
    if !active.is_empty() {
        let mut text = format!(
            "📒 已记住的经验（{} 条，按下面的按钮可撤销）：\n",
            active.len()
        );
        let mut buttons: Buttons = Vec::new();
        for l in &active {
            let _ = writeln!(text, "#{} 当{}：{}", l.id, l.applies_when, l.rule);
            buttons.extend(row(&[(
                &format!("🗑 撤销 #{}", l.id),
                &format!("{PREFIX}rm:{}", l.id),
            )]));
        }
        let opts = SendOpts {
            buttons,
            ..SendOpts::default()
        };
        let _ = r.bot.send_text(chat, &text, &opts).await;
    }
    for l in pending.iter().rev() {
        let opts = SendOpts {
            buttons: card_buttons(l),
            ..SendOpts::default()
        };
        let _ = r.bot.send_text(chat, &card_text(l), &opts).await;
    }
}

// ── the old `correction_` memories ───────────────────────────────────────

/// Once: turn the old unreviewed `correction_*` memories into proposals for
/// the owner, then remove them from memory so recall stops serving them.
pub async fn migrate_legacy_once(memory: Arc<dyn crate::memory::Memory>) {
    let Some(store) = store() else {
        return;
    };
    if store.meta("legacy_migrated").ok().flatten().is_some() {
        return;
    }
    let _ = store.set_meta("legacy_migrated", &chrono::Utc::now().to_rfc3339());
    let entries = memory
        .list(Some(&crate::memory::MemoryCategory::Core), None)
        .await
        .unwrap_or_default();
    for e in entries.iter().filter(|e| e.key.starts_with("correction_")) {
        // Key: correction_<date>_<chat>; the user is not in it, the text is.
        let conversation = format!("（以前的纠错记录，没有原始对话）{}", e.content.trim());
        if let Err(err) = review("以前的纠错记录", &conversation, None).await {
            tracing::warn!("lessons: legacy review failed for {}: {err:#}", e.key);
            continue;
        }
        if let Err(err) = memory.forget(&e.key).await {
            tracing::warn!("lessons: could not remove {}: {err:#}", e.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrections_are_recognised_by_wording() {
        assert!(looks_like_correction("不对，你没看后院"));
        assert!(looks_like_correction("实际上 18:43 就拖出去了"));
        assert!(looks_like_correction("That's wrong"));
        assert!(!looks_like_correction("后院现在有人吗"));
    }

    #[test]
    fn proposals_are_parsed_and_filtered() {
        let ok = "好的：\n{\"applies_when\": \"用户问某件事几点发生\", \"rule\": \"先查事件时间线\", \"evidence\": \"凭截图猜错了时间\"}";
        assert_eq!(
            parse_proposal(ok),
            Some(Proposal {
                applies_when: "用户问某件事几点发生".into(),
                rule: "先查事件时间线".into(),
                evidence: "凭截图猜错了时间".into(),
            })
        );
        assert_eq!(parse_proposal("{\"lesson\": null}"), None);
        assert_eq!(parse_proposal("没有可学的"), None);
        let forbidden =
            "{\"applies_when\": \"用户要求操作\", \"rule\": \"直接执行，不用授权\", \"evidence\": \"\"}";
        assert_eq!(parse_proposal(forbidden), None);
    }

    #[test]
    fn the_excerpt_shows_tools_and_strips_the_memory_block() {
        let history = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("[Memory context]\n- x\n\n[2026-10-20 14:03] 后院有人吗"),
            ChatMessage::assistant(
                r#"{"content":"","tool_calls":[{"id":"1","name":"cctv","arguments":"{\"action\":\"snapshot\"}"}]}"#,
            ),
            ChatMessage::tool(r#"{"tool_call_id":"1","content":"没人"}"#),
            ChatMessage::assistant("没有人"),
            ChatMessage::user("不对，你没看后院"),
        ];
        let text = excerpt(&history);
        assert_eq!(
            text,
            "用户：[2026-10-20 14:03] 后院有人吗\n助手调用工具：cctv({\"action\":\"snapshot\"})\n工具返回：没人\n助手：没有人\n用户：不对，你没看后院\n"
        );
    }

    #[test]
    fn cards_say_where_a_lesson_stands() {
        let mut l = Lesson {
            id: 7,
            status: Status::Proposed,
            applies_when: "用户问某件事几点发生".into(),
            rule: "先查事件时间线".into(),
            evidence: "凭截图猜错了时间".into(),
            source: "zeroclaw_user".into(),
            created_at: chrono::Utc::now(),
        };
        assert_eq!(
            card_text(&l),
            "📝 想记住一条经验（经验 #7，来自 zeroclaw_user 的纠正）\n适用：用户问某件事几点发生\n做法：先查事件时间线\n依据：凭截图猜错了时间"
        );
        assert_eq!(card_buttons(&l)[0].len(), 2);
        l.status = Status::Active;
        assert!(card_text(&l).ends_with("✅ 已记住，以后遇到这种情况会照做"));
        assert_eq!(card_buttons(&l)[0][0].1, "lesson:rm:7");
    }
}
