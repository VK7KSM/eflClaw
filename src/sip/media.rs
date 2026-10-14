//! The audio side of a call: SDP offer/answer and one RTP stream. We play a
//! prompt (looped, 20 ms G.711 frames) and listen for a DTMF digit, either
//! in-band (what the GSM gateway delivers) or as RFC 4733 events (in case the
//! server ever sends them).

use super::codec::{Codec, PT_PCMA, PT_PCMU};
use super::dtmf::DtmfDetector;
use anyhow::{bail, Context, Result};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::watch;

/// RFC 4733 telephone-event payload type we offer.
pub const PT_TELEPHONE_EVENT: u8 = 101;
const FRAME: usize = 160; // 20 ms at 8 kHz
const FRAME_TS: u32 = 160; // RTP timestamp step of one frame

pub fn offer(local_ip: IpAddr, rtp_port: u16, session_id: u64) -> String {
    format!(
        "v=0\r\n\
o=elfclaw {session_id} {session_id} IN IP4 {local_ip}\r\n\
s=elfclaw\r\n\
c=IN IP4 {local_ip}\r\n\
t=0 0\r\n\
m=audio {rtp_port} RTP/AVP {PT_PCMU} {PT_PCMA} {PT_TELEPHONE_EVENT}\r\n\
a=rtpmap:{PT_PCMU} PCMU/8000\r\n\
a=rtpmap:{PT_PCMA} PCMA/8000\r\n\
a=rtpmap:{PT_TELEPHONE_EVENT} telephone-event/8000\r\n\
a=fmtp:{PT_TELEPHONE_EVENT} 0-16\r\n\
a=ptime:20\r\n\
a=sendrecv\r\n"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub remote: SocketAddr,
    pub codec: Codec,
    pub telephone_event: Option<u8>,
}

pub fn parse_answer(sdp: &str) -> Result<Answer> {
    let mut session_ip: Option<IpAddr> = None;
    let mut media_ip: Option<IpAddr> = None;
    let mut port: Option<u16> = None;
    let mut payloads: Vec<u8> = Vec::new();
    let mut te: Option<u8> = None;
    let mut in_audio = false;
    for line in sdp.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("m=") {
            in_audio = rest.starts_with("audio");
            if in_audio && port.is_none() {
                let mut f = rest.split_whitespace();
                f.next();
                port = f.next().and_then(|p| p.parse().ok());
                f.next(); // proto
                payloads = f.filter_map(|p| p.parse::<u8>().ok()).collect();
            }
        } else if let Some(rest) = line.strip_prefix("c=") {
            let ip = rest
                .split_whitespace()
                .nth(2)
                .and_then(|a| a.split('/').next())
                .and_then(|a| a.parse().ok());
            if in_audio {
                media_ip = ip;
            } else {
                session_ip = ip;
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if in_audio && rest.to_ascii_lowercase().contains("telephone-event") {
                te = rest.split_whitespace().next().and_then(|p| p.parse().ok());
            }
        }
    }
    let ip = media_ip
        .or(session_ip)
        .context("SDP answer has no connection address")?;
    let port = port.context("SDP answer has no audio stream")?;
    if port == 0 {
        bail!("audio stream rejected in SDP answer");
    }
    let codec = payloads
        .iter()
        .find_map(|pt| Codec::from_payload_type(*pt))
        .context("SDP answer has neither PCMU nor PCMA")?;
    Ok(Answer {
        remote: SocketAddr::new(ip, port),
        codec,
        telephone_event: te.filter(|pt| payloads.contains(pt)),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaEnd {
    /// The wanted digit was pressed.
    Digit,
    /// Someone said something `speech_check` accepted.
    Voice,
    /// `speech_check` recognised a voicemail greeting: nobody picked up.
    Voicemail,
    /// Nobody responded in time.
    TimedOut,
    /// The other side hung up.
    RemoteHangup,
    /// The audio stopped (no RTP for a while) — e.g. the gateway crashed. Not
    /// a response, even if a BYE follows.
    MediaLost,
}

/// What one utterance turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// The response we want (e.g. "知道了").
    Confirmation,
    /// A voicemail greeting — end the call, nobody is there.
    Voicemail,
    /// Anything else (or not recognised): keep listening.
    Other,
}

/// Judges one utterance (8 kHz PCM). Supplied by the caller so this module
/// stays free of speech-recognition code.
pub type SpeechCheck =
    Arc<dyn Fn(Vec<i16>) -> Pin<Box<dyn Future<Output = Heard> + Send>> + Send + Sync>;

pub struct PlayOptions {
    pub prompt: Arc<Vec<i16>>,
    /// Silence between prompt repeats.
    pub gap: Duration,
    pub max: Duration,
    pub digit: char,
    /// Each utterance heard is passed here (`None`: speech is ignored).
    pub speech_check: Option<SpeechCheck>,
    /// Played once after a response, before hanging up.
    pub confirm: Option<Arc<Vec<i16>>>,
}

/// Speech checks per call at most — a voicemail greeting or a chatty line
/// must not turn into an unbounded number of model calls.
const MAX_SPEECH_CHECKS: usize = 8;
/// No RTP for this long means the media path is gone.
const MEDIA_SILENCE: Duration = Duration::from_secs(12);
/// A hang-up after this long without RTP is a dropped call, not a response.
const HANGUP_STALE: Duration = Duration::from_secs(5);

const SPEECH_RMS: f32 = 400.0;
const TONE_SHARE: f32 = 0.6;
const TONES_HZ: [f32; 3] = [400.0, 425.0, 450.0];
const PRE_ROLL_FRAMES: usize = 10; // 200 ms kept before speech starts
const END_PAUSE_MS: u32 = 500;
const MIN_SPEECH_MS: u32 = 300;
const MAX_UTTERANCE_MS: u32 = 4000;

/// Is this 20 ms frame someone talking? Loud enough, and not the 400–450 Hz
/// ringback the network plays before the phone is answered.
pub fn is_speech_frame(frame: &[i16]) -> bool {
    let x: Vec<f32> = frame.iter().map(|v| f32::from(*v)).collect();
    let energy: f32 = x.iter().map(|v| v * v).sum();
    if energy <= 0.0 {
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = x.len() as f32;
    if (energy / n).sqrt() < SPEECH_RMS {
        return false;
    }
    let tone: f32 = TONES_HZ
        .iter()
        .map(|f| {
            let c = 2.0 * (2.0 * std::f32::consts::PI * f / 8000.0).cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for v in &x {
                let s0 = v + c * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            s1 * s1 + s2 * s2 - c * s1 * s2
        })
        .sum();
    tone / (energy * n / 2.0) < TONE_SHARE
}

/// Cuts the incoming audio into utterances: from the first speech frame
/// (plus 200 ms before it) until a 500 ms pause, at most 4 s each.
#[derive(Debug, Default)]
pub struct UtteranceCollector {
    pending: Vec<i16>,
    pre_roll: std::collections::VecDeque<Vec<i16>>,
    current: Vec<i16>,
    speaking: bool,
    speech_ms: u32,
    pause_ms: u32,
}

impl UtteranceCollector {
    /// Feed decoded samples; returns the utterances completed by them.
    pub fn feed(&mut self, samples: &[i16]) -> Vec<Vec<i16>> {
        let mut done = Vec::new();
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= FRAME {
            let frame: Vec<i16> = self.pending.drain(..FRAME).collect();
            if let Some(u) = self.frame(frame) {
                done.push(u);
            }
        }
        done
    }

    fn frame(&mut self, frame: Vec<i16>) -> Option<Vec<i16>> {
        let speech = is_speech_frame(&frame);
        if !self.speaking {
            if speech {
                self.speaking = true;
                self.current = self.pre_roll.drain(..).flatten().collect();
                self.current.extend_from_slice(&frame);
                self.speech_ms = 20;
                self.pause_ms = 0;
            } else {
                self.pre_roll.push_back(frame);
                if self.pre_roll.len() > PRE_ROLL_FRAMES {
                    self.pre_roll.pop_front();
                }
            }
            return None;
        }
        self.current.extend_from_slice(&frame);
        if speech {
            self.speech_ms += 20;
            self.pause_ms = 0;
        } else {
            self.pause_ms += 20;
        }
        #[allow(clippy::cast_possible_truncation)]
        let length_ms = (self.current.len() / 8) as u32;
        if self.pause_ms >= END_PAUSE_MS || length_ms >= MAX_UTTERANCE_MS {
            self.speaking = false;
            let utterance = std::mem::take(&mut self.current);
            return (self.speech_ms >= MIN_SPEECH_MS).then_some(utterance);
        }
        None
    }
}

/// Play `opts.prompt` on a loop until the far end responds (`opts.digit`, or
/// an utterance `opts.speech_check` accepts), hangs up (`hangup` turns true),
/// the audio stops, or `opts.max` passes.
pub async fn play_and_listen(
    socket: &UdpSocket,
    answer: &Answer,
    opts: &PlayOptions,
    mut hangup: watch::Receiver<bool>,
) -> Result<MediaEnd> {
    let mut remote = answer.remote;
    let codec = answer.codec;
    let ssrc: u32 = rand::random();
    let mut seq: u16 = rand::random();
    let mut ts: u32 = rand::random();
    let mut first = true;
    let gap = vec![0i16; usize::try_from(opts.gap.as_millis()).unwrap_or(0) * 8];
    let mut source: Vec<i16> = opts.prompt.iter().copied().chain(gap).collect();
    if source.is_empty() {
        source = vec![0; FRAME];
    }
    let mut pos = 0usize;
    let mut digits = DtmfDetector::new();
    let mut utterances = UtteranceCollector::default();
    let (check_tx, mut check_rx) = tokio::sync::mpsc::unbounded_channel::<Heard>();
    let mut checks_started = 0usize;
    let mut last_event_ts: Option<u32> = None;
    // The response, once heard, and how much of `confirm` is left to play.
    let mut heard: Option<MediaEnd> = None;
    let mut confirm_left = 0usize;
    let started = Instant::now();
    let mut last_rx = started;
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 2048];
    let mut latched = false;

    let respond = |end: MediaEnd, heard: &mut Option<MediaEnd>, confirm_left: &mut usize| {
        if heard.is_none() {
            *heard = Some(end);
            *confirm_left = opts.confirm.as_ref().map_or(0, |c| c.len());
        }
    };

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let frame: Vec<i16> = match (heard, opts.confirm.as_ref()) {
                    // After the response, play the confirmation once, then stop.
                    (Some(end), Some(confirm)) => {
                        if confirm_left == 0 {
                            return Ok(end);
                        }
                        let start = confirm.len() - confirm_left;
                        let stop = (start + FRAME).min(confirm.len());
                        confirm_left = confirm.len() - stop;
                        let mut f = confirm[start..stop].to_vec();
                        f.resize(FRAME, 0);
                        f
                    }
                    (Some(end), None) => return Ok(end),
                    (None, _) => {
                        let mut f = Vec::with_capacity(FRAME);
                        for _ in 0..FRAME {
                            f.push(source[pos]);
                            pos = (pos + 1) % source.len();
                        }
                        f
                    }
                };
                let mut pkt = Vec::with_capacity(12 + FRAME);
                pkt.push(0x80);
                pkt.push(if first { 0x80 } else { 0 } | codec.payload_type());
                pkt.extend_from_slice(&seq.to_be_bytes());
                pkt.extend_from_slice(&ts.to_be_bytes());
                pkt.extend_from_slice(&ssrc.to_be_bytes());
                pkt.extend(frame.iter().map(|s| codec.encode(*s)));
                // A send error (e.g. ICMP unreachable) is not fatal for one frame.
                let _ = socket.send_to(&pkt, remote).await;
                first = false;
                seq = seq.wrapping_add(1);
                ts = ts.wrapping_add(FRAME_TS);
                if heard.is_none() {
                    if last_rx.elapsed() >= MEDIA_SILENCE {
                        return Ok(MediaEnd::MediaLost);
                    }
                    if started.elapsed() >= opts.max {
                        return Ok(MediaEnd::TimedOut);
                    }
                }
            }
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { continue };
                // Symmetric RTP: answer to wherever the media really comes from.
                if !latched && from.ip() == remote.ip() {
                    remote = from;
                    latched = true;
                }
                let Some((pt, rtp_ts, payload)) = rtp_payload(&buf[..n]) else { continue };
                last_rx = Instant::now();
                if heard.is_some() {
                    continue;
                }
                if Some(pt) == answer.telephone_event {
                    // RFC 4733: one event spans many packets with the same timestamp.
                    let digit = payload.first().and_then(|e| event_digit(*e));
                    if digit.is_some() && last_event_ts != Some(rtp_ts) {
                        last_event_ts = Some(rtp_ts);
                        if digit == Some(opts.digit) {
                            respond(MediaEnd::Digit, &mut heard, &mut confirm_left);
                        }
                    }
                } else if let Some(c) = Codec::from_payload_type(pt) {
                    let samples: Vec<i16> = payload.iter().map(|b| c.decode(*b)).collect();
                    if digits.feed(&samples).contains(&opts.digit) {
                        respond(MediaEnd::Digit, &mut heard, &mut confirm_left);
                        continue;
                    }
                    let Some(check) = opts.speech_check.as_ref() else { continue };
                    for utterance in utterances.feed(&samples) {
                        if checks_started >= MAX_SPEECH_CHECKS {
                            break;
                        }
                        checks_started += 1;
                        let (check, tx) = (check.clone(), check_tx.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(check(utterance).await);
                        });
                    }
                }
            }
            Some(what) = check_rx.recv() => {
                match what {
                    Heard::Confirmation => respond(MediaEnd::Voice, &mut heard, &mut confirm_left),
                    // No confirmation prompt for a recorder: hang up at once.
                    Heard::Voicemail if heard.is_none() => return Ok(MediaEnd::Voicemail),
                    Heard::Voicemail | Heard::Other => {}
                }
            }
            changed = hangup.changed() => {
                if changed.is_err() || *hangup.borrow() {
                    return Ok(match heard {
                        Some(end) => end,
                        None if last_rx.elapsed() >= HANGUP_STALE => MediaEnd::MediaLost,
                        None => MediaEnd::RemoteHangup,
                    });
                }
            }
        }
    }
}

/// Read a call prompt: a WAV file of 8 kHz, 16-bit, mono PCM (what telephone
/// audio is; anything else is refused rather than resampled).
pub fn read_wav_8k(bytes: &[u8]) -> Result<Vec<i16>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let mut pos = 12;
    let mut format_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        let body = bytes
            .get(pos + 8..pos.saturating_add(8).saturating_add(size))
            .context("truncated WAV chunk")?;
        if id == b"fmt " {
            let field = |at: usize| u16::from_le_bytes([body[at], body[at + 1]]);
            if body.len() < 16 {
                bail!("short WAV fmt chunk");
            }
            let rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            format_ok = field(0) == 1 && field(2) == 1 && rate == 8000 && field(14) == 16;
            if !format_ok {
                bail!("WAV must be 8 kHz 16-bit mono PCM");
            }
        } else if id == b"data" {
            if !format_ok {
                bail!("WAV data before its format");
            }
            return Ok(body
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect());
        }
        pos += 8 + size + (size & 1);
    }
    bail!("WAV without audio data")
}

/// (payload type, timestamp, payload) of an RTP packet.
fn rtp_payload(p: &[u8]) -> Option<(u8, u32, &[u8])> {
    if p.len() < 12 || p[0] >> 6 != 2 {
        return None;
    }
    let cc = usize::from(p[0] & 0x0F);
    let mut off = 12 + 4 * cc;
    if p[0] & 0x10 != 0 {
        let ext = p.get(off + 2..off + 4)?;
        off += 4 + 4 * usize::from(u16::from_be_bytes([ext[0], ext[1]]));
    }
    let mut end = p.len();
    if p[0] & 0x20 != 0 {
        end = end.checked_sub(usize::from(*p.last()?))?;
    }
    let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    Some((p[1] & 0x7F, ts, p.get(off..end)?))
}

fn event_digit(e: u8) -> Option<char> {
    match e {
        0..=9 => char::from_digit(u32::from(e), 10),
        10 => Some('*'),
        11 => Some('#'),
        12..=15 => Some(char::from(b'A' + (e - 12))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::dtmf::tone;

    const ASTERISK_ANSWER: &str = "v=0\r\no=- 1 2 IN IP4 203.0.113.5\r\ns=Asterisk\r\nc=IN IP4 203.0.113.5\r\nt=0 0\r\nm=audio 12000 RTP/AVP 0 101\r\na=rtpmap:0 PCMU/8000\r\na=rtpmap:101 telephone-event/8000\r\na=fmtp:101 0-16\r\na=ptime:20\r\na=sendrecv\r\n";

    #[test]
    fn parses_an_asterisk_answer() {
        let a = parse_answer(ASTERISK_ANSWER).unwrap();
        assert_eq!(a.remote, "203.0.113.5:12000".parse().unwrap());
        assert_eq!(a.codec, Codec::Pcmu);
        assert_eq!(a.telephone_event, Some(101));
        let alaw = ASTERISK_ANSWER
            .replace("RTP/AVP 0 101", "RTP/AVP 8")
            .replace("a=rtpmap:0 PCMU", "a=rtpmap:8 PCMA");
        let a = parse_answer(&alaw).unwrap();
        assert_eq!((a.codec, a.telephone_event), (Codec::Pcma, None));
        assert!(parse_answer(&ASTERISK_ANSWER.replace("12000", "0")).is_err());
        assert!(offer("192.168.2.71".parse().unwrap(), 40000, 7)
            .contains("m=audio 40000 RTP/AVP 0 8 101"));
    }

    #[test]
    fn reads_telephone_wav_and_refuses_other_formats() {
        let wav = |rate: u32, channels: u16, samples: &[i16]| {
            let mut v = b"RIFF\0\0\0\0WAVE".to_vec();
            v.extend_from_slice(b"fmt ");
            v.extend_from_slice(&16u32.to_le_bytes());
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&channels.to_le_bytes());
            v.extend_from_slice(&rate.to_le_bytes());
            v.extend_from_slice(&(rate * 2).to_le_bytes());
            v.extend_from_slice(&2u16.to_le_bytes());
            v.extend_from_slice(&16u16.to_le_bytes());
            v.extend_from_slice(b"LIST");
            v.extend_from_slice(&3u32.to_le_bytes());
            v.extend_from_slice(b"abc\0"); // odd chunk + pad byte
            v.extend_from_slice(b"data");
            #[allow(clippy::cast_possible_truncation)]
            v.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
            for s in samples {
                v.extend_from_slice(&s.to_le_bytes());
            }
            v
        };
        assert_eq!(
            read_wav_8k(&wav(8000, 1, &[1, -2, 300])).unwrap(),
            vec![1, -2, 300]
        );
        assert!(read_wav_8k(&wav(16000, 1, &[1])).is_err());
        assert!(read_wav_8k(&wav(8000, 2, &[1])).is_err());
        assert!(read_wav_8k(b"not audio").is_err());
    }

    fn synth(secs: f32, f: &dyn Fn(f32) -> f32) -> Vec<i16> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (secs * 8000.0) as usize;
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        (0..n).map(|i| f(i as f32 / 8000.0) as i16).collect()
    }

    /// 140 Hz voiced sound with harmonics and a syllable rhythm.
    fn speech(secs: f32) -> Vec<i16> {
        let pi2 = 2.0 * std::f32::consts::PI;
        synth(secs, &|t| {
            let envelope = 0.5 + 0.5 * (pi2 * 4.0 * t).sin().abs();
            #[allow(clippy::cast_precision_loss)]
            let v = (1..8)
                .map(|h| 2500.0 / h as f32 * (pi2 * 140.0 * h as f32 * t).sin())
                .sum::<f32>();
            envelope * v
        })
    }

    /// Ringback (400+450 Hz, the Australian cadence) and comfort noise are not
    /// speech; a short phrase is one utterance; a long greeting is cut into
    /// 4 s pieces.
    #[test]
    fn utterances_are_cut_from_speech_only() {
        let pi2 = 2.0 * std::f32::consts::PI;
        let ringback = synth(6.0, &|t| {
            let c = t % 3.0;
            let on = c < 0.4 || (0.6..1.0).contains(&c);
            if on {
                5000.0 * ((pi2 * 400.0 * t).sin() + (pi2 * 450.0 * t).sin()) / 2.0
            } else {
                0.0
            }
        });
        let mut u = UtteranceCollector::default();
        assert!(u.feed(&ringback).is_empty(), "ringback");
        assert!(
            u.feed(&synth(2.0, &|t| 20.0 * (pi2 * 1000.0 * t).sin()))
                .is_empty(),
            "comfort noise"
        );

        let mut phrase = speech(1.0);
        phrase.extend(vec![0i16; 8000]);
        let got = u.feed(&phrase);
        assert_eq!(
            got.len(),
            1,
            "one phrase: {:?}",
            got.iter().map(Vec::len).collect::<Vec<_>>()
        );
        // 200 ms pre-roll + 1 s speech + the 500 ms pause that ended it.
        assert!((12800..14400).contains(&got[0].len()), "{}", got[0].len());

        let mut greeting = speech(9.0);
        greeting.extend(vec![0i16; 8000]);
        assert_eq!(u.feed(&greeting).len(), 3, "4 s + 4 s + 1 s");

        let mut blip = speech(0.2);
        blip.extend(vec![0i16; 8000]);
        assert!(u.feed(&blip).is_empty(), "too short to be a word");
    }

    /// Speech goes to the check; only an accepted utterance ends the call as
    /// `Voice`. The stream stopping before a BYE makes it `MediaLost`.
    #[tokio::test]
    async fn accepted_speech_ends_as_voice_and_a_dead_stream_as_media_lost() {
        let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answer = Answer {
            remote: far.local_addr().unwrap(),
            codec: Codec::Pcmu,
            telephone_event: None,
        };
        let us_addr = us.local_addr().unwrap();
        let send = |audio: Vec<i16>| {
            let far_addr = us_addr;
            async move {
                let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                for (i, chunk) in audio.chunks(FRAME).enumerate() {
                    let payload: Vec<u8> = chunk.iter().map(|s| Codec::Pcmu.encode(*s)).collect();
                    #[allow(clippy::cast_possible_truncation)]
                    let pkt = rtp(0, i as u16, (i * FRAME) as u32, &payload);
                    sock.send_to(&pkt, far_addr).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        };
        // The check accepts only utterances longer than 0.6 s ("知道了"),
        // standing in for the transcription.
        let check: SpeechCheck = Arc::new(|u: Vec<i16>| {
            Box::pin(async move {
                if u.len() > 4800 {
                    Heard::Confirmation
                } else {
                    Heard::Other
                }
            })
        });
        let opts = PlayOptions {
            prompt: Arc::new(vec![0; 800]),
            gap: Duration::ZERO,
            max: Duration::from_secs(10),
            digit: '1',
            speech_check: Some(check),
            confirm: None,
        };
        let mut audio = speech(0.4); // rejected by the check
        audio.extend(vec![0i16; 6000]);
        audio.extend(speech(1.0)); // accepted
        audio.extend(vec![0i16; 6000]);
        tokio::spawn(send(audio));
        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            play_and_listen(&us, &answer, &opts, rx).await.unwrap(),
            MediaEnd::Voice
        );
        drop(far);

        // Nothing arrives and then the far end "hangs up" after 6 s: a crash.
        // Fresh sockets: the sender above may still be filling the old one.
        let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answer = Answer {
            remote: far.local_addr().unwrap(),
            ..answer
        };
        let (tx, rx) = watch::channel(false);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            let _ = tx.send(true);
        });
        let quiet = PlayOptions {
            speech_check: None,
            ..opts
        };
        assert_eq!(
            play_and_listen(&us, &answer, &quiet, rx).await.unwrap(),
            MediaEnd::MediaLost
        );

        // A voicemail greeting ends the call straight away.
        let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answer = Answer {
            remote: far.local_addr().unwrap(),
            ..answer
        };
        let us_addr = us.local_addr().unwrap();
        let recorder: SpeechCheck = Arc::new(|_| Box::pin(async { Heard::Voicemail }));
        let opts = PlayOptions {
            speech_check: Some(recorder),
            max: Duration::from_secs(10),
            ..quiet
        };
        let mut greeting = speech(1.0);
        greeting.extend(vec![0i16; 6000]);
        tokio::spawn(async move {
            let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for (i, chunk) in greeting.chunks(FRAME).enumerate() {
                let payload: Vec<u8> = chunk.iter().map(|s| Codec::Pcmu.encode(*s)).collect();
                #[allow(clippy::cast_possible_truncation)]
                let pkt = rtp(0, i as u16, (i * FRAME) as u32, &payload);
                sock.send_to(&pkt, us_addr).await.unwrap();
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let (_tx, rx) = watch::channel(false);
        let began = Instant::now();
        assert_eq!(
            play_and_listen(&us, &answer, &opts, rx).await.unwrap(),
            MediaEnd::Voicemail
        );
        assert!(began.elapsed() < Duration::from_secs(5), "hung up at once");
    }

    fn rtp(pt: u8, seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, pt];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// A fake far end: answers our stream, then "presses 1" in-band after a
    /// moment. The call must end with `Digit` and the prompt must have been sent.
    #[tokio::test]
    async fn hears_an_in_band_digit_over_real_udp() {
        let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answer = Answer {
            remote: far.local_addr().unwrap(),
            codec: Codec::Pcmu,
            telephone_event: Some(101),
        };
        let us_addr = us.local_addr().unwrap();
        let far_task = tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let (n, _) = far.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 12 + FRAME, "first prompt frame");
            assert_eq!(buf[1], 0x80, "marker bit + PCMU");
            let mut audio = vec![0i16; 1600];
            audio.extend(tone('1', 200, 5000.0));
            audio.extend(vec![0i16; 800]);
            for (i, chunk) in audio.chunks(FRAME).enumerate() {
                let payload: Vec<u8> = chunk.iter().map(|s| Codec::Pcmu.encode(*s)).collect();
                #[allow(clippy::cast_possible_truncation)]
                let pkt = rtp(0, i as u16, (i * FRAME) as u32, &payload);
                far.send_to(&pkt, us_addr).await.unwrap();
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        let (_tx, rx) = watch::channel(false);
        let opts = PlayOptions {
            prompt: Arc::new(tone('5', 300, 3000.0)),
            gap: Duration::from_millis(200),
            max: Duration::from_secs(10),
            digit: '1',
            speech_check: None,
            confirm: None,
        };
        let end = play_and_listen(&us, &answer, &opts, rx).await.unwrap();
        assert_eq!(end, MediaEnd::Digit);
        far_task.await.unwrap();
    }

    #[tokio::test]
    async fn rfc4733_event_digit_times_out_and_hangup_are_reported() {
        let us = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let answer = Answer {
            remote: far.local_addr().unwrap(),
            codec: Codec::Pcma,
            telephone_event: Some(101),
        };
        let us_addr = us.local_addr().unwrap();
        // Event "1": three packets, same timestamp.
        for i in 0..3u16 {
            far.send_to(&rtp(101, i, 4000, &[1, 0x0A, 0, 160]), us_addr)
                .await
                .unwrap();
        }
        let opts = PlayOptions {
            prompt: Arc::new(vec![0; 800]),
            gap: Duration::ZERO,
            max: Duration::from_millis(400),
            digit: '1',
            speech_check: None,
            confirm: Some(Arc::new(vec![0; 320])),
        };
        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            play_and_listen(&us, &answer, &opts, rx).await.unwrap(),
            MediaEnd::Digit
        );

        let (_tx, rx) = watch::channel(false);
        assert_eq!(
            play_and_listen(&us, &answer, &opts, rx).await.unwrap(),
            MediaEnd::TimedOut
        );

        let (tx, rx) = watch::channel(false);
        let long = PlayOptions {
            max: Duration::from_secs(10),
            ..opts
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = tx.send(true);
        });
        assert_eq!(
            play_and_listen(&us, &answer, &long, rx).await.unwrap(),
            MediaEnd::RemoteHangup
        );
    }
}
