//! elfClaw 2026-10-14: home CCTV link — cctv-server's alarms to Telegram,
//! the highest ones also as a phone call + SMS (via `crate::sip`).
//!
//! Division of work agreed with the cctv project: cctv-server detects and
//! judges (its own local AI, pictures never leave the house) and computes the
//! level; elfClaw only delivers, by the owner's rules (`worker`), and answers
//! queries / controls cameras from Telegram (`tools::cctv`). Connection is one
//! way, elfClaw → cctv (long poll), contract in `C:\Dev\CCTV\docs\elfclaw-api.md`.

pub mod alarm;
pub mod api;
pub mod feedback;
pub mod format;
pub mod home;
pub mod label;
pub mod look;
pub mod panel;
pub mod store;
pub mod tg;
pub mod worker;

use crate::config::Config;
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

/// Prompt recordings for alarm calls, in `workspace/sip/` (8 kHz WAV).
const PROMPT_WAV: &str = "sip_alarm_prompt.wav";
const CONFIRM_WAV: &str = "sip_confirm.wav";

fn read_prompt(config: &Config, name: &str) -> Result<Vec<i16>> {
    let path = config.workspace_dir.join("sip").join(name);
    let bytes = std::fs::read(&path).with_context(|| format!("{} missing", path.display()))?;
    crate::sip::media::read_wav_8k(&bytes)
}

/// The phone line for the highest alarms, when SIP and an owner number are set.
fn alarm_line(
    config: &Config,
    bot: &tg::Bot,
    api: &api::CctvApi,
    chats: &[String],
) -> Option<Arc<alarm::AlarmLine>> {
    let owner = config.cctv.owner_mobile.trim();
    if owner.is_empty() {
        return None;
    }
    let Some(sip) = crate::sip::global() else {
        tracing::warn!(
            "[cctv] owner_mobile set but [sip] is not running; alarms go to Telegram only"
        );
        return None;
    };
    let prompt = match read_prompt(config, PROMPT_WAV) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("alarm call prompt unusable ({e:#}); alarms go to Telegram only");
            return None;
        }
    };
    Some(Arc::new(alarm::AlarmLine {
        bot: bot.clone(),
        api: api.clone(),
        chats: chats.to_vec(),
        owner: owner.to_string(),
        sip,
        prompt: Arc::new(prompt),
        confirm: read_prompt(config, CONFIRM_WAV).ok().map(Arc::new),
        speech: crate::sip::stt::confirmation_check(config.sip.stt_url.clone()),
    }))
}

pub const MUTE_REPLY: &str =
    "🔕 已停止向这个对话推送监控报警（摄像头照常布防、录像，cctv 自己的 ntfy 推送不受影响）。
确认有人闯入、多台摄像头掉线这类会打电话的最高级报警仍然会推送。
恢复：/cctv_on";
pub const UNMUTE_REPLY: &str = "🔔 已恢复向这个对话推送监控报警。";

/// Switch alarm pushes to `chat` off or on. Only chats in `[cctv] send_to`
/// receive pushes at all.
pub fn set_alerts(
    workspace: &Path,
    send_to: &[String],
    chat: &str,
    on: bool,
) -> Result<&'static str> {
    if !send_to.iter().any(|c| c.trim() == chat) {
        return Ok("这个对话本来就不接收监控推送。");
    }
    store::Store::open(workspace)?.set_muted(chat, !on)?;
    Ok(if on { UNMUTE_REPLY } else { MUTE_REPLY })
}

/// Bot-menu switches `/cctv_off` and `/cctv_on`, answered without the model.
/// Call before the Telegram channel starts (it sends the menu at start-up).
pub fn register_menu(config: &Config) {
    if !config.cctv.enabled {
        return;
    }
    for (command, description, on) in [
        ("cctv_off", "停止接收监控报警推送", false),
        ("cctv_on", "恢复接收监控报警推送", true),
    ] {
        let workspace = config.workspace_dir.clone();
        let send_to = config.cctv.send_to.clone();
        crate::channels::telegram::register_menu_command(
            command,
            description,
            Arc::new(
                move |chat, _sender| match set_alerts(&workspace, &send_to, chat, on) {
                    Ok(reply) => reply.to_string(),
                    Err(e) => format!("设置失败：{e:#}"),
                },
            ),
        );
    }
    panel::register_buttons(config);
    // "我在家 / 我出门" (owner's rules 2026-10-18): only the accounts that
    // receive the pushes; any one of them saying "我出门" ends it.
    let send_to = config.cctv.send_to.clone();
    let ours = move |chat: &str| send_to.iter().any(|c| c.trim() == chat);
    let home_ours = ours.clone();
    crate::channels::telegram::register_menu_command(
        "cctv_home",
        "我在家（选时长，最长 4 小时）",
        Arc::new(move |chat, _sender| {
            if home_ours(chat) {
                home::menu_home(chat)
            } else {
                "这个对话不接收监控推送。".into()
            }
        }),
    );
    crate::channels::telegram::register_menu_command(
        "cctv_away",
        "我出门",
        Arc::new(move |chat, sender| {
            if ours(chat) {
                home::menu_away(chat, sender)
            } else {
                "这个对话不接收监控推送。".into()
            }
        }),
    );
}

/// Daemon component: build everything from config and run the worker.
pub async fn run(config: Config) -> Result<()> {
    let c = &config.cctv;
    let bot_token = config
        .channels_config
        .telegram
        .as_ref()
        .map(|t| t.bot_token.clone())
        .filter(|t| !t.is_empty())
        .context("[cctv] needs the Telegram channel configured")?;
    let bot = tg::Bot::new(&bot_token);
    let tz: chrono_tz::Tz = c.timezone.parse().unwrap_or(chrono_tz::Australia::Sydney);
    let hhmm = |s: &str, default: u32| crate::config::parse_hhmm(s).unwrap_or(default);
    let chats: Vec<String> = c
        .send_to
        .iter()
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .collect();
    let cctv_api = api::CctvApi::new(&c.base_url)?;
    let ctx = Arc::new(worker::Ctx {
        alarm: alarm_line(&config, &bot, &cctv_api, &chats),
        api: cctv_api,
        bot: bot.clone(),
        workspace: config.workspace_dir.clone(),
        chats,
        tz,
        quiet: (
            hhmm(&c.quiet_start, 23 * 60),
            hhmm(&c.quiet_end, 6 * 60 + 30),
        ),
        summary_at: hhmm(&c.summary_time, 7 * 60),
    });
    crate::channels::telegram::register_callback_handler(
        alarm::ACK_PREFIX,
        Arc::new(alarm::telegram_callback),
    );
    let _ = alarm::BOT.set(bot.clone());
    home::register(
        ctx.api.clone(),
        bot.clone(),
        config.workspace_dir.clone(),
        ctx.tz,
    );
    feedback::register(ctx.api.clone(), bot.clone(), config.workspace_dir.clone());
    panel::init(
        ctx.api.clone(),
        bot.clone(),
        config.workspace_dir.clone(),
        ctx.tz,
        ctx.chats.clone(),
    );
    label::register(ctx.api.clone(), bot.clone(), config.workspace_dir.clone());
    tracing::info!(
        chats = ctx.chats.len(),
        phone = ctx.alarm.is_some(),
        "CCTV link starting"
    );
    let store = store::Store::open(&config.workspace_dir)?;
    worker::run(ctx, store).await
}
