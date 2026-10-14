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
pub mod format;
pub mod store;
pub mod tg;
pub mod worker;

use crate::config::Config;
use anyhow::{Context, Result};
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
fn alarm_line(config: &Config, bot: &tg::Bot, chats: &[String]) -> Option<Arc<alarm::AlarmLine>> {
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
        chats: chats.to_vec(),
        owner: owner.to_string(),
        sip,
        prompt: Arc::new(prompt),
        confirm: read_prompt(config, CONFIRM_WAV).ok().map(Arc::new),
        speech: crate::sip::stt::confirmation_check(config.sip.stt_url.clone()),
    }))
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
    let ctx = Arc::new(worker::Ctx {
        api: api::CctvApi::new(&c.base_url)?,
        bot: bot.clone(),
        alarm: alarm_line(&config, &bot, &chats),
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
    tracing::info!(
        chats = ctx.chats.len(),
        phone = ctx.alarm.is_some(),
        "CCTV link starting"
    );
    let store = store::Store::open(&config.workspace_dir)?;
    worker::run(ctx, store).await
}
