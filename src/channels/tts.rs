//! Text-to-Speech synthesis via Microsoft Edge Read Aloud API.
//!
//! Uses the `msedge-tts` crate which communicates with Microsoft's free
//! Edge TTS WebSocket endpoint. No API key required.

use anyhow::{Context, Result};
use msedge_tts::tts::client::connect_async;
use msedge_tts::tts::SpeechConfig;
use reqwest::multipart::{Form, Part};
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, info, warn};

use crate::config::TtsConfig;

/// Synthesize text into an MP3 audio file using Microsoft Edge TTS.
///
/// Returns the path to the generated temp file.
/// The caller is responsible for cleaning up via [`cleanup`].
pub async fn synthesize(text: &str, config: &TtsConfig) -> Result<PathBuf> {
    if text.is_empty() {
        anyhow::bail!("Cannot synthesize empty text");
    }

    // Truncate if it exceeds max_chars
    let text_to_speak = if text.chars().count() > config.max_chars {
        let truncated: String = text.chars().take(config.max_chars).collect();
        warn!(
            "TTS text truncated from {} to {} chars",
            text.chars().count(),
            config.max_chars
        );
        truncated
    } else {
        text.to_string()
    };

    let clean_text = strip_markdown_for_speech(&text_to_speak);

    debug!(
        "Synthesizing {} chars with voice {}",
        clean_text.len(),
        config.voice
    );

    let speech_config = SpeechConfig {
        voice_name: config.voice.clone(),
        audio_format: "audio-24khz-48kbitrate-mono-mp3".to_string(),
        rate: config.rate,
        pitch: config.pitch,
        volume: 0,
    };

    let mut tts = connect_async()
        .await
        .context("Failed to connect to Edge TTS service")?;

    let audio = tts
        .synthesize(&clean_text, &speech_config)
        .await
        .context("Edge TTS synthesis failed")?;

    if audio.audio_bytes.is_empty() {
        anyhow::bail!("Edge TTS returned empty audio");
    }

    let tmp_dir = std::env::temp_dir();
    let file_name = format!("zeroclaw_tts_{}.mp3", uuid::Uuid::new_v4());
    let file_path = tmp_dir.join(&file_name);

    fs::write(&file_path, &audio.audio_bytes)
        .await
        .context("Failed to write TTS audio file")?;

    info!(
        "TTS synthesized {} chars → {} bytes → {}",
        clean_text.len(),
        audio.audio_bytes.len(),
        file_path.display()
    );

    Ok(file_path)
}

/// Send a voice message directly via the Telegram Bot API.
///
/// This is a standalone function that doesn't require access to `TelegramChannel` internals.
pub async fn send_voice_to_telegram(
    bot_token: &str,
    chat_id: &str,
    audio_path: &Path,
) -> Result<()> {
    let file_bytes = fs::read(audio_path).await?;
    let file_name = audio_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("voice.mp3")
        .to_string();

    let part = Part::bytes(file_bytes).file_name(file_name.clone());

    let form = Form::new()
        .text("chat_id", chat_id.to_string())
        .part("voice", part);

    let url = format!("https://api.telegram.org/bot{bot_token}/sendVoice");

    let client = crate::config::build_runtime_proxy_client("tts.telegram");
    let resp = client.post(&url).multipart(form).send().await?;

    if !resp.status().is_success() {
        let err = resp.text().await?;
        anyhow::bail!("Telegram sendVoice failed: {err}");
    }

    info!("TTS voice sent to {chat_id}: {file_name}");
    Ok(())
}

/// Synthesize text to OGG Opus bytes for the Xiaozhi device (24 kHz mono Opus).
///
/// Edge TTS returns a native OGG Opus container when the format is set to
/// `ogg-24khz-16bit-mono-opus`, so no re-encoding is needed.
/// The bytes can be sent directly via the Xiaozhi WebSocket protocol.
pub async fn synthesize_to_ogg_opus(text: &str, config: &TtsConfig) -> Result<Vec<u8>> {
    if text.is_empty() {
        anyhow::bail!("Cannot synthesize empty text");
    }

    let text_to_speak = if text.chars().count() > config.max_chars {
        let truncated: String = text.chars().take(config.max_chars).collect();
        warn!(
            "TTS text truncated from {} to {} chars",
            text.chars().count(),
            config.max_chars
        );
        truncated
    } else {
        text.to_string()
    };

    let clean_text = strip_markdown_for_speech(&text_to_speak);

    debug!(
        "Synthesizing {} chars (OGG Opus 24kHz) with voice {}",
        clean_text.len(),
        config.voice
    );

    let speech_config = SpeechConfig {
        voice_name: config.voice.clone(),
        audio_format: "ogg-24khz-16bit-mono-opus".to_string(),
        rate: config.rate,
        pitch: config.pitch,
        volume: 0,
    };

    let mut tts = connect_async()
        .await
        .context("Failed to connect to Edge TTS service")?;

    let audio = tts
        .synthesize(&clean_text, &speech_config)
        .await
        .context("Edge TTS synthesis (OGG Opus) failed")?;

    if audio.audio_bytes.is_empty() {
        anyhow::bail!("Edge TTS returned empty audio (OGG Opus)");
    }

    info!(
        "TTS OGG Opus: {} chars → {} bytes",
        clean_text.len(),
        audio.audio_bytes.len()
    );

    Ok(audio.audio_bytes)
}

/// Clean up a TTS audio file after sending.
pub async fn cleanup(path: &Path) {
    if let Err(e) = fs::remove_file(path).await {
        warn!("Failed to clean up TTS file {}: {e}", path.display());
    }
}

/// Strip common Markdown formatting to produce cleaner speech output.
fn strip_markdown_for_speech(text: &str) -> String {
    let mut result = String::with_capacity(text.len());

    for line in text.lines() {
        let trimmed = line.trim();

        // Skip horizontal rules
        if trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___") {
            continue;
        }

        // Strip heading markers
        let line = if trimmed.starts_with('#') {
            trimmed.trim_start_matches('#').trim()
        } else {
            trimmed
        };

        // Strip bold/italic markers
        let line = line.replace("**", "").replace("__", "");
        let line = line.replace('*', "").replace('_', "");

        // Strip inline code backticks
        let line = line.replace('`', "");

        // Strip bullet markers
        let line = if line.starts_with("- ") || line.starts_with("• ") {
            line[2..].to_string()
        } else {
            line
        };

        if !line.trim().is_empty() {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(line.trim());
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_markdown_removes_formatting() {
        let input = "## Hello\n\n**Bold** and *italic* text\n\n- item one\n- item two";
        let result = strip_markdown_for_speech(input);
        assert_eq!(result, "Hello Bold and italic text item one item two");
    }

    #[test]
    fn strip_markdown_removes_code_backticks() {
        let input = "Use `cargo build` to compile";
        let result = strip_markdown_for_speech(input);
        assert_eq!(result, "Use cargo build to compile");
    }

    #[test]
    fn strip_markdown_skips_horizontal_rules() {
        let input = "Before\n---\nAfter";
        let result = strip_markdown_for_speech(input);
        assert_eq!(result, "Before After");
    }
}
