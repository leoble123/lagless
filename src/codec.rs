//! Frame encoders/decoders. PCM is the zero-delay default; Opus is for thin links (Bluetooth, bad Wi-Fi).

use crate::protocol::{Codec, AUDIO_HEADER_LEN, FRAME_HEADER_LEN, MAX_PACKET};
use anyhow::{bail, Result};

pub struct Encoder {
    codec: Codec,
    dither: u32,
    #[cfg(feature = "opus")]
    opus: Option<opus::Encoder>,
}

impl Encoder {
    pub fn new(codec: Codec, sample_rate: u32, channels: usize, bitrate_kbps: u32) -> Result<Self> {
        #[cfg(not(feature = "opus"))]
        let _ = (sample_rate, channels, bitrate_kbps);
        #[cfg(feature = "opus")]
        let opus = if codec == Codec::Opus {
            if sample_rate != 48000 {
                bail!("opus needs a 48 kHz source (yours is {sample_rate} Hz); use pcm16");
            }
            let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            let mut e = opus::Encoder::new(48000, ch, opus::Application::LowDelay)?;
            e.set_bitrate(opus::Bitrate::Bits((bitrate_kbps * 1000) as i32))?;
            Some(e)
        } else {
            None
        };
        #[cfg(not(feature = "opus"))]
        if codec == Codec::Opus {
            bail!("this build has no opus support; rebuild with default features");
        }
        Ok(Encoder {
            codec,
            dither: 0x1234_5678,
            #[cfg(feature = "opus")]
            opus,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Samples per channel in one frame. Frame length is a direct latency cost on the sender
    /// (it has to collect a whole frame before sending), so PCM uses very short frames, capped
    /// so a frame always fits in one datagram even at 192 kHz.
    pub fn frame_samples(&self, sample_rate: u32, channels: usize, frame_ms: f64) -> usize {
        match self.codec {
            Codec::Pcm16 | Codec::Pcm24 => {
                let bytes = if self.codec == Codec::Pcm16 { 2 } else { 3 };
                let fit = (MAX_PACKET - AUDIO_HEADER_LEN - FRAME_HEADER_LEN) / (bytes * channels);
                ((sample_rate as f64 * frame_ms / 1000.0).round() as usize).clamp(16, fit)
            }
            // 5 ms halves the packet rate vs 2.5 ms, which matters on Bluetooth.
            Codec::Opus => 240,
        }
    }

    pub fn encode(&mut self, pcm: &[f32], out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        match self.codec {
            Codec::Pcm16 => {
                out.reserve(pcm.len() * 2);
                for &s in pcm {
                    // TPDF dither so quiet passages don't turn into distortion at 16 bits.
                    let d = (self.rand() - self.rand()) / 32768.0;
                    let v = ((s + d) * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            Codec::Pcm24 => {
                out.reserve(pcm.len() * 3);
                for &s in pcm {
                    let v = (s * 8_388_607.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                    out.extend_from_slice(&v.to_le_bytes()[..3]);
                }
            }
            Codec::Opus => {
                #[cfg(feature = "opus")]
                {
                    out.resize(1500, 0);
                    let n = self.opus.as_mut().unwrap().encode_float(pcm, out)?;
                    out.truncate(n);
                }
            }
        }
        Ok(())
    }

    fn rand(&mut self) -> f32 {
        self.dither = self.dither.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.dither >> 8) as f32 / (1u32 << 24) as f32
    }
}

pub struct Decoder {
    codec: Codec,
    channels: usize,
    #[cfg(feature = "opus")]
    opus: Option<opus::Decoder>,
}

impl Decoder {
    pub fn new(codec: Codec, channels: usize) -> Result<Self> {
        #[cfg(feature = "opus")]
        let opus = if codec == Codec::Opus {
            let ch = if channels == 1 { opus::Channels::Mono } else { opus::Channels::Stereo };
            Some(opus::Decoder::new(48000, ch)?)
        } else {
            None
        };
        #[cfg(not(feature = "opus"))]
        if codec == Codec::Opus {
            bail!("sender is using opus but this build has no opus support");
        }
        Ok(Decoder {
            codec,
            channels,
            #[cfg(feature = "opus")]
            opus,
        })
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<()> {
        out.clear();
        match self.codec {
            Codec::Pcm16 => out.extend(
                data.chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0),
            ),
            Codec::Pcm24 => out.extend(data.chunks_exact(3).map(|b| {
                (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0
            })),
            Codec::Opus => {
                #[cfg(feature = "opus")]
                {
                    out.resize(5760 * self.channels, 0.0);
                    let n = self.opus.as_mut().unwrap().decode_float(data, out, false)?;
                    out.truncate(n * self.channels);
                }
            }
        }
        if out.len() % self.channels != 0 {
            bail!("frame length not a multiple of channel count");
        }
        Ok(())
    }

    /// Codec-native packet-loss concealment. Returns false if the codec has none.
    pub fn conceal(&mut self, frame_samples: usize, out: &mut Vec<f32>) -> bool {
        #[cfg(feature = "opus")]
        if let Some(d) = self.opus.as_mut() {
            out.resize(frame_samples * self.channels, 0.0);
            if let Ok(n) = d.decode_float(&[], out, false) {
                out.truncate(n * self.channels);
                return true;
            }
        }
        let _ = (frame_samples, out);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(codec: Codec, tol: f32) {
        let pcm: Vec<f32> = (0..240).map(|i| ((i as f32) * 0.05).sin() * 0.8).collect();
        let mut enc = Encoder::new(codec, 48000, 2, 320).unwrap();
        let mut dec = Decoder::new(codec, 2).unwrap();
        let mut bytes = Vec::new();
        let mut out = Vec::new();
        enc.encode(&pcm, &mut bytes).unwrap();
        dec.decode(&bytes, &mut out).unwrap();
        assert_eq!(out.len(), pcm.len());
        for (a, b) in pcm.iter().zip(&out) {
            assert!((a - b).abs() < tol, "{a} vs {b}");
        }
    }

    #[test]
    fn pcm16() {
        roundtrip(Codec::Pcm16, 2.0 / 32768.0 + 1e-6);
    }

    #[test]
    fn pcm24() {
        roundtrip(Codec::Pcm24, 1.0 / 8_000_000.0 + 1e-7);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn opus_decodes_and_conceals() {
        let mut enc = Encoder::new(Codec::Opus, 48000, 2, 320).unwrap();
        let mut dec = Decoder::new(Codec::Opus, 2).unwrap();
        let pcm = vec![0.1f32; 480];
        let mut bytes = Vec::new();
        let mut out = Vec::new();
        enc.encode(&pcm, &mut bytes).unwrap();
        dec.decode(&bytes, &mut out).unwrap();
        assert_eq!(out.len(), 480);
        assert!(dec.conceal(240, &mut out));
        assert_eq!(out.len(), 480);
    }
}
