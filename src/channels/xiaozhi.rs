//! Xiaozhi voice terminal channel — AI-VOX3 (ESP32-S3) WebSocket protocol server.
//!
//! Supports the Xiaozhi WebSocket protocol used by nulllab AI-VOX3 devices (firmware 1.9.0).
//!
//! # Protocol flow (confirmed against v1.9.0 firmware source)
//!
//! ```text
//! Device → POST /xiaozhi/ota/  (board JSON)
//! Server → {"websocket":{"url":"ws://...","version":1}}
//!
//! Device → WebSocket upgrade  (headers: Protocol-Version:1, Device-Id, Client-Id)
//! Device → {"type":"hello","version":1,"transport":"websocket","audio_params":{"format":"opus","sample_rate":16000,"channels":1,"frame_duration":60}}
//! Server → {"type":"hello","transport":"websocket","session_id":"...","audio_params":{"sample_rate":24000,"frame_duration":60}}
//!
//! Device → {"type":"listen","state":"start","mode":"realtime"}
//! Device → [Binary v1 raw Opus frames, 16kHz mono, 60ms/frame, continuous]
//!          [1-byte DTX silence frames when user is quiet]
//! Server detects N consecutive 1-byte frames → end-of-speech (VAD)
//!   OR Device → {"type":"listen","state":"stop"}  (auto mode)
//!
//! Server → {"type":"stt","text":"transcribed text"}
//! Server → {"type":"tts","state":"start"}
//! Server → [Binary raw Opus frames at 24kHz mono from Edge TTS]
//! Server → {"type":"tts","state":"stop"}
//! ```
//!
//! # Key facts (verified by Python test server with real device)
//!
//! - Protocol version **1** (raw Opus, no frame header).
//! - Device sends `listen:start mode=realtime` immediately on connect; no button press needed.
//! - `stt:start` ACK is **not required** — device streams frames immediately after `listen:start`.
//! - `tts:idle` is **not handled** by the firmware — do not send it.
//! - `audio_params.sample_rate` in server hello = server **downstream** TTS rate (24000), not device recording rate.
//!
//! # Setup (device redirect without firmware changes)
//!
//! 1. Device enters AP mode (hold GPIO0 or GPIO47).
//! 2. Connect to `AI-VOX-XXXX` AP → `192.168.4.1`.
//! 3. Set OTA URL to `http://<server_ip>:42619/xiaozhi/ota/`.
//! 4. elfClaw OTA mock returns elfClaw's WebSocket address.

use crate::channels::traits::{Channel, ChannelMessage, SendMessage};
use crate::channels::transcription::transcribe_audio;
use crate::channels::tts::synthesize_to_ogg_opus;
use crate::config::{TranscriptionConfig, TtsConfig};
use anyhow::Result;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum frame size (bytes) that counts as a DTX silence/comfort-noise packet.
/// Opus DTX sends exactly 1 byte (the TOC byte only) when the encoder decides
/// the frame is silent.
const DTX_MAX_BYTES: usize = 1;

/// Number of consecutive DTX silence frames before we consider speech ended.
/// 8 frames × 60 ms = 480 ms of silence — robust against mid-sentence pauses.
const DTX_SILENCE_TRIGGER: usize = 8;

// ── Config ────────────────────────────────────────────────────────────────────

/// Configuration for the Xiaozhi AI-VOX3 voice channel (`[channels_config.xiaozhi]`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct XiaozhiConfig {
    /// WebSocket server listen port. Default: 42618.
    #[serde(default = "default_xiaozhi_port")]
    pub port: u16,
    /// WebSocket server listen address. Default: "0.0.0.0".
    #[serde(default = "default_xiaozhi_host")]
    pub host: String,
    /// OTA mock HTTP server port (device reads this to find the WebSocket URL). Default: 42619.
    #[serde(default = "default_xiaozhi_ota_port")]
    pub ota_port: u16,
    /// Server IP advertised to the device in OTA JSON.
    /// Defaults to the `host` value, or `127.0.0.1` when host is a wildcard.
    #[serde(default)]
    pub server_ip: Option<String>,
}

fn default_xiaozhi_port() -> u16 {
    42618
}
fn default_xiaozhi_host() -> String {
    "0.0.0.0".to_string()
}
fn default_xiaozhi_ota_port() -> u16 {
    42619
}

impl crate::config::traits::ChannelConfig for XiaozhiConfig {
    fn name() -> &'static str {
        "Xiaozhi"
    }
    fn desc() -> &'static str {
        "Xiaozhi AI-VOX3 voice terminal (WebSocket + Opus)"
    }
}

// ── Channel struct ────────────────────────────────────────────────────────────

/// Xiaozhi WebSocket voice channel.
pub struct XiaozhiChannel {
    config: XiaozhiConfig,
    transcription: TranscriptionConfig,
    tts: TtsConfig,
    /// device_id → TTS audio sender (OGG Opus bytes).
    active_sessions: Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
}

impl XiaozhiChannel {
    pub fn new(config: XiaozhiConfig, transcription: TranscriptionConfig, tts: TtsConfig) -> Self {
        Self {
            config,
            transcription,
            tts,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Resolve the IP address to advertise to the device in OTA JSON.
    fn server_ip(&self) -> String {
        self.config.server_ip.clone().unwrap_or_else(|| {
            if self.config.host == "0.0.0.0" || self.config.host == "::" {
                "127.0.0.1".to_string()
            } else {
                self.config.host.clone()
            }
        })
    }
}

// ── OGG helpers ───────────────────────────────────────────────────────────────

/// Wrap raw Opus frames into an OGG Opus container (RFC 7845) for Groq STT.
///
/// The device sends bare Opus frames at 16 kHz mono. Groq Whisper requires
/// them in an OGG container. We build a minimal valid stream with:
/// - OpusHead packet (19 bytes)
/// - OpusTags packet (minimal, 0 user comments)
/// - One OGG page per audio packet
fn wrap_opus_frames_in_ogg(frames: &[Vec<u8>], sample_rate: u32) -> Result<Vec<u8>> {
    use ogg::writing::{PacketWriteEndInfo, PacketWriter};

    let mut out = Vec::new();
    let mut writer = PacketWriter::new(Cursor::new(&mut out));
    let stream_serial: u32 = 1;

    // OpusHead — RFC 7845 §5.1
    let sr = sample_rate.to_le_bytes();
    let opus_head: Vec<u8> = vec![
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', // 0-7 magic
        1,    // 8  version
        1,    // 9  channel count (mono)
        0x38, 0x01, // 10-11 pre-skip = 312 LE (standard for Opus)
        sr[0], sr[1], sr[2], sr[3], // 12-15 input sample rate LE
        0x00, 0x00, // 16-17 output gain = 0
        0,    // 18 channel mapping family = 0 (mono)
    ];
    writer.write_packet(opus_head, stream_serial, PacketWriteEndInfo::EndPage, 0)?;

    // OpusTags — RFC 7845 §5.2 (minimal: vendor string + 0 comments)
    let vendor = b"elfClaw";
    let mut tags: Vec<u8> = Vec::with_capacity(8 + 4 + vendor.len() + 4);
    tags.extend_from_slice(b"OpusTags");
    tags.extend_from_slice(&u32::try_from(vendor.len()).unwrap_or(0).to_le_bytes());
    tags.extend_from_slice(vendor);
    tags.extend_from_slice(&0u32.to_le_bytes()); // 0 user comment fields
    writer.write_packet(tags, stream_serial, PacketWriteEndInfo::EndPage, 0)?;

    // Audio packets — granule positions are always in 48 kHz samples for OGG Opus.
    // Device uses frame_duration=60ms → 60ms × 48 kHz / 1000 = 2880 samples per frame.
    let samples_per_frame: u64 = 2880;
    let mut granule: u64 = 0;
    let total = frames.len();
    for (i, frame) in frames.iter().enumerate() {
        granule += samples_per_frame;
        let end_info = if i + 1 == total {
            PacketWriteEndInfo::EndStream
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(frame.clone(), stream_serial, end_info, granule)?;
    }

    drop(writer);
    Ok(out)
}

/// Send OGG Opus audio to the WebSocket sink as binary frames (one Opus packet per frame).
///
/// Skips the first two OGG packets (OpusHead + OpusTags) per RFC 7845 §3.
async fn send_ogg_to_ws<S>(sink: &mut S, ogg_bytes: &[u8]) -> Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    use ogg::reading::PacketReader;

    let mut reader = PacketReader::new(Cursor::new(ogg_bytes));
    let mut idx: usize = 0;
    loop {
        match reader.read_packet() {
            Ok(Some(pkt)) => {
                if idx >= 2 {
                    // skip OpusHead and OpusTags
                    sink.send(Message::Binary(pkt.data.to_vec().into())).await?;
                }
                idx += 1;
            }
            Ok(None) => break,
            Err(e) => {
                warn!("Xiaozhi: OGG read error: {e}");
                break;
            }
        }
    }
    Ok(())
}

// ── OTA mock HTTP server ──────────────────────────────────────────────────────

/// Serve a minimal OTA endpoint so the device can discover the WebSocket URL
/// without firmware changes.
///
/// Responds to any TCP connection with an HTTP 200 JSON response containing
/// the WebSocket address.
async fn serve_ota(host: String, ota_port: u16, ws_url: String) {
    let addr = format!("{host}:{ota_port}");
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Xiaozhi: OTA HTTP bind {addr} failed: {e}");
            return;
        }
    };
    info!("Xiaozhi: OTA mock HTTP on http://{addr}/xiaozhi/ota/");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("Xiaozhi: OTA request from {peer}");
                let url = ws_url.clone();
                tokio::spawn(async move {
                    respond_ota(stream, url).await;
                });
            }
            Err(e) => warn!("Xiaozhi: OTA accept error: {e}"),
        }
    }
}

/// Handle one OTA HTTP request: read past the headers, write JSON response.
async fn respond_ota(mut stream: tokio::net::TcpStream, ws_url: String) {
    // Read until double CRLF to consume the HTTP request headers.
    let mut buf = [0u8; 1];
    let mut header_bytes: Vec<u8> = Vec::with_capacity(512);
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                header_bytes.push(buf[0]);
                if header_bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
                if header_bytes.len() > 8192 {
                    break; // safety limit
                }
            }
        }
    }

    let body = serde_json::json!({
        "websocket": {
            "url": ws_url,
            "version": 1   // force v1: raw Opus, no frame header
        }
    })
    .to_string();

    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );

    let _ = stream.write_all(response.as_bytes()).await;
}

// ── WebSocket connection handler ──────────────────────────────────────────────

/// Handle one Xiaozhi device connection end-to-end.
async fn handle_connection(
    tcp: tokio::net::TcpStream,
    transcription: TranscriptionConfig,
    message_tx: mpsc::Sender<ChannelMessage>,
    sessions: Arc<Mutex<HashMap<String, mpsc::Sender<Vec<u8>>>>>,
) {
    let ws = match tokio_tungstenite::accept_async(tcp).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!("Xiaozhi: WebSocket handshake failed: {e}");
            return;
        }
    };

    let (mut sink, mut source) = ws.split();

    // ── Phase 1: hello handshake ─────────────────────────────────────────────
    let device_id = loop {
        let msg = match source.next().await {
            Some(Ok(m)) => m,
            _ => return,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return,
            _ => continue,
        };
        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if json["type"].as_str() != Some("hello") {
            continue;
        }

        let id = json["device_id"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("xiaozhi-{}", Uuid::new_v4()));

        let sid = Uuid::new_v4().to_string();
        // Protocol v1 confirmed by test server against real device (firmware 1.9.0).
        // audio_params.sample_rate = server downstream TTS rate (24000), not device recording rate.
        // transport must be "websocket" — device checks this field explicitly.
        let hello = serde_json::json!({
            "type": "hello",
            "transport": "websocket",
            "session_id": sid,
            "audio_params": {
                "sample_rate": 24000,
                "frame_duration": 60
            }
        });
        if sink
            .send(Message::Text(hello.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
        break id;
    };

    // Register per-device TTS channel.
    let (tts_tx, mut tts_rx) = mpsc::channel::<Vec<u8>>(4);
    sessions.lock().insert(device_id.clone(), tts_tx);
    info!("Xiaozhi: device {device_id} connected");

    // ── Phase 2: main conversation loop ─────────────────────────────────────
    'outer: loop {
        // Wait for listen:start (or abort/close)
        let mut frames: Vec<Vec<u8>> = Vec::new();

        loop {
            tokio::select! {
                // Branch 1: device messages (listen:start / detect / abort / close)
                ws_msg = source.next() => {
                    let msg = match ws_msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            warn!("Xiaozhi: {device_id} WS error: {e}");
                            break 'outer;
                        }
                        None => {
                            debug!("Xiaozhi: {device_id} stream closed");
                            break 'outer;
                        }
                    };
                    match msg {
                        Message::Text(t) => {
                            let json: serde_json::Value = match serde_json::from_str(&t) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            match json["type"].as_str() {
                                Some("listen") if json["state"].as_str() == Some("start") => {
                                    let sid = json["session_id"].as_str().unwrap_or("").to_string();
                                    let mode = json["mode"].as_str().unwrap_or("auto");
                                    info!("Xiaozhi: {device_id} listen:start mode={mode} session={sid}");
                                    // No stt:start ACK needed — device streams frames immediately
                                    // after listen:start regardless of any server acknowledgement.
                                    break; // start collecting frames
                                }
                                Some("listen") if json["state"].as_str() == Some("detect") => {
                                    debug!("Xiaozhi: {device_id} listen:detect (waiting for user input)");
                                }
                                Some("abort") => {
                                    debug!("Xiaozhi: {device_id} abort (idle)");
                                }
                                _ => {}
                            }
                        }
                        Message::Ping(data) => {
                            let _ = sink.send(Message::Pong(data)).await;
                        }
                        Message::Close(frame) => {
                            debug!("Xiaozhi: {device_id} device sent Close: {:?}", frame);
                            break 'outer;
                        }
                        _ => {}
                    }
                }
                // Branch 2: proactive push (heartbeat/agent calls XiaozhiChannel::send while device is idle)
                Some(proactive_ogg) = tts_rx.recv() => {
                    debug!("Xiaozhi: {device_id} proactive TTS push while idle");
                    let start = serde_json::json!({"type": "tts", "state": "start"});
                    let _ = sink.send(Message::Text(start.to_string().into())).await;
                    if let Err(e) = send_ogg_to_ws(&mut sink, &proactive_ogg).await {
                        warn!("Xiaozhi: {device_id} proactive TTS stream error: {e}");
                    }
                    let stop = serde_json::json!({"type": "tts", "state": "stop"});
                    let _ = sink.send(Message::Text(stop.to_string().into())).await;
                    // Continue waiting for next listen:start — do not break.
                }
            }
        }

        // Collect Opus frames until end-of-speech is detected.
        //
        // realtime mode (default): device streams continuously and never sends listen:stop.
        //   End-of-speech is detected when DTX_SILENCE_TRIGGER consecutive 1-byte DTX
        //   silence frames arrive (device Opus encoder emits 1-byte comfort-noise when silent).
        //
        // auto mode: device explicitly sends listen:stop; we handle it for compatibility.
        let stt_text = {
            let mut silence_streak: usize = 0;
            loop {
                let msg = match tokio::time::timeout(
                    tokio::time::Duration::from_secs(30),
                    source.next(),
                )
                .await
                {
                    Ok(Some(Ok(m))) => m,
                    Ok(Some(Err(e))) => {
                        warn!("Xiaozhi: {device_id} WS error in frame collection: {e}");
                        break 'outer;
                    }
                    Ok(None) => {
                        debug!("Xiaozhi: {device_id} stream closed during collection");
                        break 'outer;
                    }
                    Err(_) => {
                        // 30 s timeout — if we collected any audio, proceed; otherwise skip.
                        if frames.is_empty() {
                            warn!("Xiaozhi: {device_id} frame collection timeout, no audio — skipping");
                            continue 'outer;
                        }
                        info!(
                            "Xiaozhi: {device_id} frame collection timeout with {} frames — proceeding to STT",
                            frames.len()
                        );
                        break;
                    }
                };
                match msg {
                    Message::Binary(data) => {
                        if data.len() <= DTX_MAX_BYTES {
                            // 1-byte DTX silence frame — count streaks, do not push to audio buffer.
                            silence_streak += 1;
                            if silence_streak >= DTX_SILENCE_TRIGGER {
                                info!(
                                    "Xiaozhi: {device_id} VAD silence detected \
                                     ({silence_streak} DTX frames, {} audio frames collected)",
                                    frames.len()
                                );
                                break; // end-of-speech → proceed to STT
                            }
                        } else {
                            silence_streak = 0;
                            frames.push(data.to_vec());
                        }
                    }
                    Message::Text(t) => {
                        let json: serde_json::Value = match serde_json::from_str(&t) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        match json["type"].as_str() {
                            // auto mode: device explicitly signals end of speech.
                            Some("listen") if json["state"].as_str() == Some("stop") => {
                                info!(
                                    "Xiaozhi: {device_id} listen:stop ({} frames collected)",
                                    frames.len()
                                );
                                break;
                            }
                            Some("abort") => {
                                debug!("Xiaozhi: {device_id} abort (listening)");
                                continue 'outer;
                            }
                            _ => {}
                        }
                    }
                    Message::Ping(data) => {
                        let _ = sink.send(Message::Pong(data)).await;
                    }
                    Message::Close(_) => break 'outer,
                    _ => {}
                }
            }

            if frames.is_empty() {
                warn!("Xiaozhi: {device_id} listen:stop received but no audio frames — skipping");
                continue 'outer;
            }

            // Wrap raw Opus frames in an OGG container for Groq STT.
            let ogg = match wrap_opus_frames_in_ogg(&frames, 16000) {
                Ok(b) => b,
                Err(e) => {
                    error!("Xiaozhi: OGG wrapping failed: {e}");
                    continue 'outer;
                }
            };

            match transcribe_audio(ogg, "voice.ogg", &transcription).await {
                Ok(t) => {
                    let t = t.trim().to_string();
                    if t.is_empty() {
                        warn!("Xiaozhi: {device_id} STT returned empty result — skipping");
                        continue 'outer;
                    }
                    t
                }
                Err(e) => {
                    warn!("Xiaozhi: STT failed: {e}");
                    continue 'outer;
                }
            }
        }; // end 'collect

        info!("Xiaozhi: {device_id} → \"{stt_text}\"");

        // Echo STT result to device display.
        let stt_msg = serde_json::json!({"type": "stt", "text": stt_text});
        let _ = sink.send(Message::Text(stt_msg.to_string().into())).await;

        // Forward to agent loop.
        let channel_msg = ChannelMessage {
            id: format!("xiaozhi-{}", Uuid::new_v4()),
            sender: device_id.clone(),
            reply_target: device_id.clone(),
            content: stt_text,
            channel: "xiaozhi".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: None,
        };
        if message_tx.send(channel_msg).await.is_err() {
            break 'outer;
        }

        // Wait for TTS audio (agent calls XiaozhiChannel::send → pushes to tts_tx).
        let ogg_response =
            match tokio::time::timeout(tokio::time::Duration::from_secs(30), tts_rx.recv()).await {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    warn!("Xiaozhi: TTS channel closed for {device_id}");
                    break 'outer;
                }
                Err(_) => {
                    warn!("Xiaozhi: TTS timeout for {device_id}");
                    continue 'outer;
                }
            };

        // Stream TTS audio frames to device.
        let start = serde_json::json!({"type": "tts", "state": "start"});
        let _ = sink.send(Message::Text(start.to_string().into())).await;
        if let Err(e) = send_ogg_to_ws(&mut sink, &ogg_response).await {
            warn!("Xiaozhi: TTS stream error: {e}");
        }
        let stop = serde_json::json!({"type": "tts", "state": "stop"});
        let _ = sink.send(Message::Text(stop.to_string().into())).await;
    }

    // Cleanup session on disconnect.
    sessions.lock().remove(&device_id);
    info!("Xiaozhi: device {device_id} disconnected");
}

// ── Channel trait impl ────────────────────────────────────────────────────────

#[async_trait]
impl Channel for XiaozhiChannel {
    fn name(&self) -> &str {
        "xiaozhi"
    }

    /// Send agent response to the device: synthesize TTS audio and push to its session.
    async fn send(&self, message: &SendMessage) -> Result<()> {
        let device_id = &message.recipient;

        let ogg_bytes = synthesize_to_ogg_opus(&message.content, &self.tts).await?;

        let sender = self.active_sessions.lock().get(device_id).cloned();
        match sender {
            Some(tx) => tx
                .send(ogg_bytes)
                .await
                .map_err(|_| anyhow::anyhow!("Xiaozhi: session {device_id} already closed")),
            None => {
                warn!("Xiaozhi: no active session for device {device_id} — response dropped");
                Ok(())
            }
        }
    }

    /// Start the WebSocket server (and OTA mock HTTP server).
    ///
    /// This is a long-running future; the caller runs it in a spawned task.
    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> Result<()> {
        let host = self.config.host.clone();
        let port = self.config.port;
        let ota_port = self.config.ota_port;
        let server_ip = self.server_ip();
        let transcription = self.transcription.clone();
        let sessions = Arc::clone(&self.active_sessions);

        // Spawn OTA mock HTTP (device uses this to find our WebSocket URL).
        let ws_url = format!("ws://{server_ip}:{port}");
        tokio::spawn(serve_ota(host.clone(), ota_port, ws_url));

        let addr = format!("{host}:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        info!("Xiaozhi: WebSocket server listening on ws://{addr}");

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    warn!("Xiaozhi: accept error: {e}");
                    continue;
                }
            };
            debug!("Xiaozhi: TCP connect from {peer}");

            let transcription = transcription.clone();
            let tx = tx.clone();
            let sessions = Arc::clone(&sessions);

            tokio::spawn(handle_connection(stream, transcription, tx, sessions));
        }
    }

    /// Verify the WebSocket port is reachable.
    async fn health_check(&self) -> bool {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        tokio::net::TcpStream::connect(&addr).await.is_ok()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{TranscriptionConfig, TtsConfig};

    #[test]
    fn wrap_opus_frames_produces_ogg_magic() {
        let frames: Vec<Vec<u8>> = vec![
            vec![0xFC, 0xFF, 0xFE], // minimal Opus SILK frame header bytes (not real audio)
            vec![0xFC, 0xAA, 0xBB],
        ];
        let result = wrap_opus_frames_in_ogg(&frames, 16000);
        assert!(result.is_ok(), "OGG wrapping should succeed: {:?}", result);
        let ogg = result.unwrap();
        // Every OGG file starts with the OggS capture pattern.
        assert!(ogg.starts_with(b"OggS"), "output must start with OggS");
        // Should be non-trivial (header + tags + audio pages).
        assert!(ogg.len() > 60, "OGG output too small: {} bytes", ogg.len());
    }

    #[test]
    fn wrap_empty_frames_fails_gracefully() {
        // Writing zero audio packets still produces valid header pages.
        // PacketWriter won't emit EndStream, but that's acceptable for our use case
        // (empty input → we don't forward to STT).
        let frames: Vec<Vec<u8>> = vec![];
        let result = wrap_opus_frames_in_ogg(&frames, 16000);
        // Either Ok (writer closes cleanly) or Err — both are acceptable.
        // The important thing is no panic.
        let _ = result;
    }

    #[test]
    fn xiaozhi_config_defaults() {
        let config: XiaozhiConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.port, 42618);
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.ota_port, 42619);
        assert!(config.server_ip.is_none());
    }

    #[test]
    fn server_ip_wildcard_returns_localhost() {
        let ch = XiaozhiChannel::new(
            XiaozhiConfig {
                port: 42618,
                host: "0.0.0.0".to_string(),
                ota_port: 42619,
                server_ip: None,
            },
            TranscriptionConfig::default(),
            TtsConfig::default(),
        );
        assert_eq!(ch.server_ip(), "127.0.0.1");
    }

    #[test]
    fn server_ip_uses_explicit_value() {
        let ch = XiaozhiChannel::new(
            XiaozhiConfig {
                port: 42618,
                host: "0.0.0.0".to_string(),
                ota_port: 42619,
                server_ip: Some("192.168.1.100".to_string()),
            },
            TranscriptionConfig::default(),
            TtsConfig::default(),
        );
        assert_eq!(ch.server_ip(), "192.168.1.100");
    }

    #[test]
    fn channel_name_is_xiaozhi() {
        let ch = XiaozhiChannel::new(
            XiaozhiConfig {
                port: 42618,
                host: "0.0.0.0".to_string(),
                ota_port: 42619,
                server_ip: None,
            },
            TranscriptionConfig::default(),
            TtsConfig::default(),
        );
        assert_eq!(ch.name(), "xiaozhi");
    }
}
