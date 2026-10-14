//! In-band DTMF detection (Goertzel), 8 kHz linear PCM.
//!
//! The GSM gateway turns the owner's key presses into ordinary audio, so the
//! digit has to be heard in the RTP stream rather than read from RFC 4733
//! events. Standard approach: 205-sample blocks (25.6 ms), the power at the
//! eight DTMF frequencies, and a digit is reported once it has been present
//! for two blocks in a row and then released or kept — never twice for one
//! press.

const SAMPLE_RATE: f32 = 8000.0;
const BLOCK: usize = 205;
const ROWS: [f32; 4] = [697.0, 770.0, 852.0, 941.0];
const COLS: [f32; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
const KEYS: [[char; 4]; 4] = [
    ['1', '2', '3', 'A'],
    ['4', '5', '6', 'B'],
    ['7', '8', '9', 'C'],
    ['*', '0', '#', 'D'],
];

/// Share of the block's energy that must sit in the two tones.
const MIN_TONE_SHARE: f32 = 0.55;
/// Quietest block considered at all (RMS ≈ 120 of 32768).
const MIN_MEAN_SQUARE: f32 = 120.0 * 120.0;
/// Allowed level difference between the two tones (≈ 8 dB either way).
const MAX_TWIST: f32 = 6.3;
/// The strongest row/column must beat the runner-up by ≈ 6 dB.
const MIN_PEAK_RATIO: f32 = 4.0;
/// Consecutive blocks a digit must last to count (≈ 51 ms).
const HITS_NEEDED: u8 = 2;

#[derive(Debug)]
pub struct DtmfDetector {
    buf: Vec<f32>,
    coeffs: [f32; 8],
    candidate: Option<char>,
    hits: u8,
    /// Digit already reported and still held down.
    latched: Option<char>,
}

impl Default for DtmfDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl DtmfDetector {
    pub fn new() -> Self {
        let mut coeffs = [0.0; 8];
        for (i, f) in ROWS.iter().chain(COLS.iter()).enumerate() {
            coeffs[i] = 2.0 * (2.0 * std::f32::consts::PI * f / SAMPLE_RATE).cos();
        }
        Self {
            buf: Vec::with_capacity(BLOCK),
            coeffs,
            candidate: None,
            hits: 0,
            latched: None,
        }
    }

    /// Feed samples; returns the digits that were pressed in them.
    pub fn feed(&mut self, samples: &[i16]) -> Vec<char> {
        let mut found = Vec::new();
        for &s in samples {
            self.buf.push(f32::from(s));
            if self.buf.len() == BLOCK {
                if let Some(d) = self.block() {
                    found.push(d);
                }
                self.buf.clear();
            }
        }
        found
    }

    fn block(&mut self) -> Option<char> {
        let digit = self.classify();
        match digit {
            Some(d) if Some(d) == self.candidate => {
                self.hits = self.hits.saturating_add(1);
            }
            Some(d) => {
                self.candidate = Some(d);
                self.hits = 1;
            }
            None => {
                self.candidate = None;
                self.hits = 0;
                self.latched = None;
                return None;
            }
        }
        if self.hits >= HITS_NEEDED && self.latched != self.candidate {
            self.latched = self.candidate;
            return self.candidate;
        }
        None
    }

    fn classify(&self) -> Option<char> {
        #[allow(clippy::cast_precision_loss)]
        let n = self.buf.len() as f32;
        let energy: f32 = self.buf.iter().map(|x| x * x).sum();
        if energy / n < MIN_MEAN_SQUARE {
            return None;
        }
        let mut power = [0.0f32; 8];
        for (i, c) in self.coeffs.iter().enumerate() {
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in &self.buf {
                let s0 = x + c * s1 - s2;
                s2 = s1;
                s1 = s0;
            }
            power[i] = s1 * s1 + s2 * s2 - c * s1 * s2;
        }
        let (row, row_p, row_2nd) = strongest(&power[..4]);
        let (col, col_p, col_2nd) = strongest(&power[4..]);
        // A pure tone of amplitude A gives |X|² ≈ (N·A/2)² against an energy of
        // N·A²/2, i.e. |X|² ≈ energy·N/2; two tones split the energy.
        let share = (row_p + col_p) / (energy * n / 2.0);
        let twist = row_p / col_p.max(f32::MIN_POSITIVE);
        let ok = share >= MIN_TONE_SHARE
            && (1.0 / MAX_TWIST..=MAX_TWIST).contains(&twist)
            && row_p >= MIN_PEAK_RATIO * row_2nd
            && col_p >= MIN_PEAK_RATIO * col_2nd;
        ok.then(|| KEYS[row][col])
    }
}

/// (index, power, second-highest power).
fn strongest(p: &[f32]) -> (usize, f32, f32) {
    let mut best = 0;
    for i in 1..p.len() {
        if p[i] > p[best] {
            best = i;
        }
    }
    let second = p
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best)
        .map(|(_, v)| *v)
        .fold(0.0f32, f32::max);
    (best, p[best], second)
}

/// Synthesise a DTMF digit (tests, and as a reference for the thresholds).
#[cfg(test)]
pub(crate) fn tone(digit: char, ms: usize, amplitude: f32) -> Vec<i16> {
    let (row, col) = KEYS
        .iter()
        .enumerate()
        .find_map(|(row, keys)| keys.iter().position(|k| *k == digit).map(|col| (row, col)))
        .expect("DTMF key");
    (0..ms * 8)
        .map(|i| {
            #[allow(clippy::cast_precision_loss)]
            let time = i as f32 / SAMPLE_RATE;
            let value = amplitude
                * ((2.0 * std::f32::consts::PI * ROWS[row] * time).sin()
                    + (2.0 * std::f32::consts::PI * COLS[col] * time).sin());
            #[allow(clippy::cast_possible_truncation)]
            let sample = value as i16;
            sample
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::codec::Codec;

    fn silence(ms: usize) -> Vec<i16> {
        vec![0; ms * 8]
    }

    #[test]
    fn every_key_is_recognised_once_per_press() {
        for row in KEYS {
            for key in row {
                let mut d = DtmfDetector::new();
                let mut audio = silence(100);
                audio.extend(tone(key, 120, 6000.0));
                audio.extend(silence(100));
                assert_eq!(d.feed(&audio), vec![key], "{key}");
            }
        }
    }

    #[test]
    fn a_long_press_counts_once_and_two_presses_count_twice() {
        let mut d = DtmfDetector::new();
        let mut audio = tone('1', 600, 6000.0);
        assert_eq!(d.feed(&audio), vec!['1']);
        audio = silence(80);
        audio.extend(tone('1', 100, 6000.0));
        assert_eq!(d.feed(&audio), vec!['1']);
    }

    #[test]
    fn survives_g711_and_background_noise() {
        for codec in [Codec::Pcmu, Codec::Pcma] {
            let mut d = DtmfDetector::new();
            let mut seed = 7u32;
            let audio: Vec<i16> = tone('1', 150, 4000.0)
                .into_iter()
                .map(|s| {
                    // Cheap deterministic noise, about -25 dB below the tone.
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    #[allow(clippy::cast_possible_wrap)]
                    let noise = ((seed >> 16) as i16 % 400) - 200;
                    codec.decode(codec.encode(s.saturating_add(noise)))
                })
                .collect();
            assert_eq!(d.feed(&audio), vec!['1'], "{codec:?}");
        }
    }

    #[test]
    fn speech_like_audio_and_single_tones_are_not_digits() {
        let mut d = DtmfDetector::new();
        // A 440 Hz tone with harmonics (a voice-ish signal) and a lone 697 Hz tone.
        let voice: Vec<i16> = (0..8000)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / SAMPLE_RATE;
                let v = 3000.0 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
                    + 2000.0 * (2.0 * std::f32::consts::PI * 880.0 * t).sin()
                    + 1000.0 * (2.0 * std::f32::consts::PI * 1320.0 * t).sin();
                #[allow(clippy::cast_possible_truncation)]
                let s = v as i16;
                s
            })
            .collect();
        assert!(d.feed(&voice).is_empty());
        let lone: Vec<i16> = (0..4000)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / SAMPLE_RATE;
                #[allow(clippy::cast_possible_truncation)]
                let s = (6000.0 * (2.0 * std::f32::consts::PI * 697.0 * t).sin()) as i16;
                s
            })
            .collect();
        assert!(d.feed(&lone).is_empty());
        // Too short a blip (one block) is ignored.
        let mut blip = silence(50);
        blip.extend(tone('5', 28, 6000.0));
        blip.extend(silence(50));
        assert!(d.feed(&blip).is_empty());
    }
}
