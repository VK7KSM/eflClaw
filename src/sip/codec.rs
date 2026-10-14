//! G.711 μ-law (PCMU, payload 0) and A-law (PCMA, payload 8), the two codecs
//! the SIP server offers that need no library. 8 kHz, one byte per sample.

/// RTP payload types used in the SDP offer.
pub const PT_PCMU: u8 = 0;
pub const PT_PCMA: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Pcmu,
    Pcma,
}

impl Codec {
    pub fn from_payload_type(pt: u8) -> Option<Self> {
        match pt {
            PT_PCMU => Some(Self::Pcmu),
            PT_PCMA => Some(Self::Pcma),
            _ => None,
        }
    }

    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => PT_PCMU,
            Self::Pcma => PT_PCMA,
        }
    }

    pub fn encode(self, sample: i16) -> u8 {
        match self {
            Self::Pcmu => linear_to_ulaw(sample),
            Self::Pcma => linear_to_alaw(sample),
        }
    }

    pub fn decode(self, byte: u8) -> i16 {
        match self {
            Self::Pcmu => ulaw_to_linear(byte),
            Self::Pcma => alaw_to_linear(byte),
        }
    }

    /// The byte that decodes to (near) silence.
    pub fn silence(self) -> u8 {
        self.encode(0)
    }
}

const ULAW_BIAS: i32 = 0x84;
const ULAW_CLIP: i32 = 32635;

pub fn linear_to_ulaw(sample: i16) -> u8 {
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(ULAW_CLIP) + ULAW_BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && s & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let byte = (sign | (exponent << 4) | mantissa) as u8;
    !byte
}

pub fn ulaw_to_linear(byte: u8) -> i16 {
    let b = i32::from(!byte);
    let sign = b & 0x80;
    let exponent = (b >> 4) & 0x07;
    let mantissa = b & 0x0F;
    let magnitude = (((mantissa << 3) + ULAW_BIAS) << exponent) - ULAW_BIAS;
    #[allow(clippy::cast_possible_truncation)]
    let v = if sign != 0 { -magnitude } else { magnitude } as i16;
    v
}

pub fn linear_to_alaw(sample: i16) -> u8 {
    let mut s = i32::from(sample);
    let sign = if s >= 0 {
        0x80
    } else {
        s = -s - 1;
        0
    };
    s = s.min(32767);
    let byte = if s < 256 {
        s >> 4
    } else {
        let mut exponent = 7;
        let mut mask = 0x4000;
        while s & mask == 0 {
            exponent -= 1;
            mask >>= 1;
        }
        (exponent << 4) | ((s >> (exponent + 3)) & 0x0F)
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let out = ((byte | sign) ^ 0x55) as u8;
    out
}

pub fn alaw_to_linear(byte: u8) -> i16 {
    let b = i32::from(byte ^ 0x55);
    let sign = b & 0x80;
    let exponent = (b >> 4) & 0x07;
    let mantissa = b & 0x0F;
    let magnitude = if exponent == 0 {
        (mantissa << 4) + 8
    } else {
        ((mantissa << 4) + 0x108) << (exponent - 1)
    };
    #[allow(clippy::cast_possible_truncation)]
    let v = if sign != 0 { magnitude } else { -magnitude } as i16;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_stay_within_quantisation_error() {
        for codec in [Codec::Pcmu, Codec::Pcma] {
            for s in (-32000i32..32000).step_by(97) {
                #[allow(clippy::cast_possible_truncation)]
                let s = s as i16;
                let back = codec.decode(codec.encode(s));
                let err = (i32::from(back) - i32::from(s)).abs();
                // Logarithmic coding: error grows with magnitude, ~3% plus a floor.
                let allowed = i32::from(s).abs() / 16 + 40;
                assert!(err <= allowed, "{codec:?} {s} -> {back}");
            }
        }
    }

    #[test]
    fn known_reference_values() {
        assert_eq!(linear_to_ulaw(0), 0xFF);
        assert_eq!(ulaw_to_linear(0xFF), 0);
        assert_eq!(linear_to_alaw(0), 0xD5);
        assert!(alaw_to_linear(0xD5).abs() <= 8);
        assert!(ulaw_to_linear(linear_to_ulaw(i16::MAX)) > 30000);
        assert!(ulaw_to_linear(linear_to_ulaw(i16::MIN)) < -30000);
    }
}
