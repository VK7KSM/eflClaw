//! Local speech-to-text for call confirmations — the owner's own model on
//! K5 (an X-ASR zipformer fine-tuned on radio Mandarin), served by the
//! resident subtitle node at `http://127.0.0.1:8766/stt` with the model
//! already loaded. Nothing leaves the machine.
//!
//! Protocol: POST a WAV body; the reply is one JSON object, `{"text": ...}`
//! or `{"error": ...}`. Clips up to 30 s.

use anyhow::{bail, Context, Result};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
/// The streaming model drops a short phrase at the very end of a clip unless
/// silence follows it (measured: a bare 1 s "知道了" came back empty; with
/// 0.3 s before and 1 s after it was recognised).
const LEAD_SILENCE: usize = 2400;
const TRAIL_SILENCE: usize = 8000;

/// Transcribe 8 kHz PCM with the recogniser at `url`.
pub async fn transcribe(url: &str, samples: &[i16]) -> Result<String> {
    let mut padded = vec![0i16; LEAD_SILENCE];
    padded.extend_from_slice(samples);
    padded.extend(std::iter::repeat_n(0i16, TRAIL_SILENCE));
    let resp = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .no_proxy()
        .build()?
        .post(url)
        .header("Content-Type", "audio/wav")
        .body(wav_from_pcm(&padded, 8000))
        .send()
        .await
        .context("speech recogniser unreachable")?;
    let body = resp
        .text()
        .await
        .context("speech recogniser reply unreadable")?;
    parse_reply(&body)
}

fn parse_reply(body: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).context("bad recogniser reply")?;
    if let Some(err) = v["error"].as_str() {
        bail!("speech recognition failed: {err}");
    }
    Ok(v["text"]
        .as_str()
        .context("recogniser reply without text")?
        .trim()
        .to_string())
}

/// 16-bit mono PCM wrapped as a WAV file.
pub fn wav_from_pcm(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX);
    let mut v = Vec::with_capacity(44 + samples.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// The speech check for alarm calls: transcribe locally, then tell a
/// confirmation from a voicemail greeting. `None` when no recogniser is set.
pub fn confirmation_check(url: String) -> Option<super::media::SpeechCheck> {
    if url.trim().is_empty() {
        return None;
    }
    let url = std::sync::Arc::new(url);
    Some(std::sync::Arc::new(move |utterance: Vec<i16>| {
        let url = url.clone();
        Box::pin(async move {
            match transcribe(&url, &utterance).await {
                Ok(text) => {
                    let heard = super::classify_speech(&text);
                    tracing::info!(text = %text, ?heard, "call speech recognised");
                    heard
                }
                Err(e) => {
                    tracing::warn!("call speech not recognised: {e:#}");
                    super::media::Heard::Other
                }
            }
        })
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_wrapper_round_trips_through_the_call_reader() {
        let pcm = vec![0i16, 1000, -1000, 32767];
        let wav = wav_from_pcm(&pcm, 8000);
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(crate::sip::media::read_wav_8k(&wav).unwrap(), pcm);
    }

    #[test]
    fn recogniser_reply_is_read() {
        assert_eq!(
            parse_reply("{\"text\":\"知道了\",\"secs\":2.3,\"ms\":95}\n").unwrap(),
            "知道了"
        );
        assert!(parse_reply("{\"error\":\"not a wav\"}").is_err());
        assert!(parse_reply("").is_err());
    }

    /// A stand-in recogniser over real HTTP: the padded WAV arrives as the
    /// body, the JSON reply comes back as a classification.
    #[tokio::test]
    async fn posts_the_clip_and_reads_the_answer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stt", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 65536];
                // Read until the whole body (header says how long) is in.
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len: usize = text[..h]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        if req.len() >= h + 4 + len {
                            assert_eq!(&req[h + 4..h + 8], b"RIFF");
                            assert_eq!(len, 44 + (2400 + 800 + 8000) * 2, "padded clip");
                            break;
                        }
                    }
                }
                let body = "{\"text\":\"知道了\",\"secs\":1.4,\"ms\":80}";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        assert_eq!(transcribe(&url, &[0i16; 800]).await.unwrap(), "知道了");
        let check = confirmation_check(url).unwrap();
        assert_eq!(
            check(vec![0i16; 800]).await,
            crate::sip::media::Heard::Confirmation
        );
        assert!(confirmation_check(String::new()).is_none());
    }
}
