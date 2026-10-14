//! elfClaw 2026-10-14: SIP component — the owner's phone line.
//!
//! Registers as one extension on the owner's Asterisk server (TLS only) and
//! offers two things to the rest of elfClaw:
//!
//! - **SMS**: `send_sms` (SIP MESSAGE, which the server hands to its GSM
//!   gateway) and `subscribe_sms` for replies. The server routes a reply back
//!   to the extension that last texted that number.
//! - **Alarm calls**: `call` dials a mobile through the gateway, loops a
//!   spoken prompt and waits for a key press. The gateway answers at once and
//!   only then rings the phone, so "answered" means nothing; a press of the
//!   wanted digit (detected in the audio, see `dtmf`) is the only proof that a
//!   person picked up.
//!
//! Small on purpose: one account, one TLS connection, outgoing calls only,
//! G.711 audio. See `client` for what is and is not implemented.

pub mod client;
pub mod codec;
pub mod digest;
pub mod dtmf;
pub mod media;
pub mod message;
pub mod stt;

pub use client::{SipAccount, SipClient};

use std::sync::OnceLock;

static GLOBAL: OnceLock<SipClient> = OnceLock::new();

/// The client the daemon started, if SIP is configured.
pub fn global() -> Option<SipClient> {
    GLOBAL.get().cloned()
}

/// Does a transcription say "got it"? Whole phrases only: "您好" in a
/// voicemail greeting must not count as "好".
pub fn is_confirmation(text: &str) -> bool {
    let t = normalised(text);
    const WORDS: [&str; 12] = [
        "知道", "收到", "明白", "好的", "好了", "确认", "了解", "ok", "okay", "gotit", "yes",
        "roger",
    ];
    !t.is_empty() && WORDS.iter().any(|w| t.contains(w))
}

/// Phrases of a voicemail greeting (Australian carriers, English and Chinese).
const VOICEMAIL_PHRASES: [&str; 12] = [
    "leaveamessage",
    "notavailable",
    "afterthetone",
    "voicemail",
    "messagebank",
    "unabletotakeyourcall",
    "留言",
    "无法接听",
    "无法接通",
    "暂时无法",
    "语音信箱",
    "提示音后",
];

fn normalised(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && !c.is_ascii_punctuation())
        .flat_map(char::to_lowercase)
        .collect()
}

/// What a transcription of the callee's side is. A greeting is checked first:
/// "please leave a message, OK?" is still a recorder.
pub fn classify_speech(text: &str) -> media::Heard {
    let t = normalised(text);
    if VOICEMAIL_PHRASES.iter().any(|p| t.contains(p)) {
        media::Heard::Voicemail
    } else if is_confirmation(text) {
        media::Heard::Confirmation
    } else {
        media::Heard::Other
    }
}

/// Make `client` the process-wide client. Only the first call wins.
pub fn install(client: SipClient) {
    let _ = GLOBAL.set(client);
}

/// Daemon start-up: when `[sip]` is enabled, read the credentials file, start
/// the client (it stays registered for the life of the process, reconnecting
/// on its own) and make it the global one. Problems are logged, not fatal —
/// the rest of elfClaw runs without a phone line.
pub fn start_from_config(config: &crate::config::Config) {
    let sip = &config.sip;
    if !sip.enabled {
        return;
    }
    let account = std::fs::read_to_string(&sip.credentials_file)
        .map_err(anyhow::Error::from)
        .and_then(|text| SipAccount::from_credentials(&text, &sip.server, sip.port));
    match account {
        Ok(account) if !sip.server.trim().is_empty() => {
            tracing::info!(user = %account.user, server = %sip.server, "SIP line starting");
            let client = SipClient::start(account);
            install(client.clone());
            tokio::spawn(async move {
                // Health: ok while registered, error while not.
                let mut was: Option<bool> = None;
                loop {
                    let now = client.is_registered();
                    if was != Some(now) {
                        if now {
                            crate::health::mark_component_ok("sip");
                        } else {
                            crate::health::mark_component_error("sip", "not registered");
                        }
                        was = Some(now);
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            });
        }
        Ok(_) => tracing::warn!("[sip] enabled but no server configured; SIP line not started"),
        Err(e) => tracing::warn!(
            "SIP credentials unreadable ({}): {e:#}",
            sip.credentials_file
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_speech, is_confirmation, media::Heard};

    #[test]
    fn confirmations_are_recognised_and_voicemail_greetings_are_not() {
        for yes in [
            "知道了",
            "知道了。",
            "嗯，收到",
            "好的好的",
            "OK",
            "okay got it",
            "明白",
        ] {
            assert!(is_confirmation(yes), "{yes}");
        }
        for no in [
            "",
            "您好，您拨打的用户暂时无法接通，请在提示音后留言。",
            "The person you are calling is not available. Please leave a message after the tone.",
            "喂？",
        ] {
            assert!(!is_confirmation(no), "{no}");
        }
    }

    #[test]
    fn voicemail_greetings_are_told_apart() {
        // Heard on the owner's phone, 2026-10-14.
        let real =
            "The person you have called is not available. Please leave a message after the tone.";
        assert_eq!(classify_speech(real), Heard::Voicemail);
        assert_eq!(
            classify_speech("您拨打的用户暂时无法接听， 请在提示音后留言"),
            Heard::Voicemail
        );
        assert_eq!(classify_speech("知道了"), Heard::Confirmation);
        assert_eq!(classify_speech("喂？"), Heard::Other);
        assert_eq!(classify_speech(""), Heard::Other);
    }
}
