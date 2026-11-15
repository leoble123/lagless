//! "Voice focus" for the mic link: keep voices, drop the room.
//!
//! high-pass (rumble, hum, desk thumps) -> RNNoise (neural noise suppression: keyboards, fans,
//! AC, chatter) -> voice-activated leveler (brings quiet speech up, never pumps up background)
//! -> soft gate between phrases -> limiter. Runs on 10 ms blocks at 48 kHz mono.

use nnnoiseless::DenoiseState;
use std::collections::VecDeque;

const FRAME: usize = DenoiseState::FRAME_SIZE; // 480 = 10 ms @ 48 kHz
/// Total added delay: one block of buffering plus RNNoise's own overlap (measured in tests).
pub const DELAY_SECS: f64 = 2.0 * FRAME as f64 / 48_000.0;

/// Target speech loudness (RMS) and how far the leveler may move.
const TARGET_RMS: f32 = 0.1; // ≈ -20 dBFS
const MAX_GAIN: f32 = 8.0; // +18 dB for quiet talkers / far mics
const MIN_GAIN: f32 = 0.5; // -6 dB for shouting
/// Between phrases the residual room sound is pushed down this much more.
const GATE_FLOOR: f32 = 0.25; // -12 dB

/// 2nd-order Butterworth high-pass (RBJ cookbook), direct form I.
struct HighPass {
    b: [f32; 3],
    a: [f32; 2],
    x: [f32; 2],
    y: [f32; 2],
}

impl HighPass {
    fn new(cutoff: f32, rate: f32) -> Self {
        let w = std::f32::consts::TAU * cutoff / rate;
        let alpha = w.sin() * std::f32::consts::FRAC_1_SQRT_2; // sin(w) / (2Q), Q = 1/sqrt(2)
        let cos = w.cos();
        let a0 = 1.0 + alpha;
        HighPass {
            b: [(1.0 + cos) / 2.0 / a0, -(1.0 + cos) / a0, (1.0 + cos) / 2.0 / a0],
            a: [-2.0 * cos / a0, (1.0 - alpha) / a0],
            x: [0.0; 2],
            y: [0.0; 2],
        }
    }

    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

pub struct VoiceFocus {
    rnn: Box<DenoiseState<'static>>,
    hp: HighPass,
    input: Vec<f32>,
    output: VecDeque<f32>,
    scratch: [f32; FRAME],
    gain: f32,
    gate: f32,
}

impl VoiceFocus {
    /// None unless the mic runs at 48 kHz (what RNNoise is trained on).
    pub fn new(rate: u32) -> Option<Self> {
        (rate == 48_000).then(|| VoiceFocus {
            rnn: DenoiseState::new(),
            hp: HighPass::new(90.0, 48_000.0),
            input: Vec::with_capacity(FRAME),
            // Primed with one block of silence: output keeps pace with input at a fixed delay.
            output: std::iter::repeat_n(0.0, FRAME).collect(),
            scratch: [0.0; FRAME],
            gain: 1.0,
            gate: 1.0,
        })
    }

    /// Processes mono samples in place (same length out as in, constant delay).
    pub fn process(&mut self, mono: &mut [f32]) {
        for s in mono.iter_mut() {
            self.input.push(self.hp.tick(*s) * 32768.0); // RNNoise wants 16-bit scale
            if self.input.len() == FRAME {
                self.block();
            }
            *s = self.output.pop_front().unwrap_or(0.0);
        }
    }

    fn block(&mut self) {
        let vad = self.rnn.process_frame(&mut self.scratch, &self.input);
        self.input.clear();

        let rms = (self.scratch.iter().map(|x| (x / 32768.0).powi(2)).sum::<f32>() / FRAME as f32).sqrt();
        // Level only on confident speech, so background never gets turned up.
        if vad > 0.6 && rms > 1e-4 {
            let want = (TARGET_RMS / rms).clamp(MIN_GAIN, MAX_GAIN);
            self.gain += (want - self.gain) * 0.06; // ~150 ms to settle
        }
        let gate_to = if vad > 0.5 {
            1.0
        } else if vad < 0.2 {
            GATE_FLOOR
        } else {
            GATE_FLOOR + (1.0 - GATE_FLOOR) * (vad - 0.2) / 0.3
        };
        // Open fast (don't clip the first syllable), close slowly (don't chop word endings).
        let gate_end = if gate_to > self.gate { gate_to } else { self.gate + (gate_to - self.gate) * 0.15 };

        for (i, x) in self.scratch.iter().enumerate() {
            let t = i as f32 / FRAME as f32;
            let g = self.gain * (self.gate + (gate_end - self.gate) * t);
            self.output.push_back(soft_limit(x / 32768.0 * g));
        }
        self.gate = gate_end;
    }
}

/// Transparent below 0.9, then rounds off smoothly toward 1.0 instead of clipping.
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.9 {
        x
    } else {
        x.signum() * (0.9 + 0.1 * ((a - 0.9) / 0.1).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    fn noise(n: usize, amp: f32, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Crude voiced speech: 120 Hz glottal-ish pulse train through vowel formants, syllable envelope.
    fn speechy(n: usize, amp: f32) -> Vec<f32> {
        let formants = [(700.0f32, 1.0f32), (1200.0, 0.6), (2600.0, 0.3)];
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                let f0 = 120.0 + 15.0 * (t * 3.0 * std::f32::consts::TAU).sin();
                let env = (t * 4.0 * std::f32::consts::PI).sin().abs();
                let mut v = 0.0;
                for h in 1..30 {
                    let f = f0 * h as f32;
                    let w: f32 = formants.iter().map(|(c, g)| g / (1.0 + ((f - c) / 120.0).powi(2))).sum();
                    v += w * (std::f32::consts::TAU * f * t).sin() / h as f32;
                }
                v * env * amp
            })
            .collect()
    }

    #[test]
    fn needs_48k() {
        assert!(VoiceFocus::new(44_100).is_none());
        assert!(VoiceFocus::new(48_000).is_some());
    }

    #[test]
    fn same_length_and_fixed_delay() {
        let mut vf = VoiceFocus::new(48_000).unwrap();
        // Odd-sized chunks like the sender's frames: whatever goes in comes out, one block later.
        for n in [48, 64, 100, 7, 480, 961] {
            let mut buf = vec![0.0; n];
            vf.process(&mut buf);
            assert_eq!(buf.len(), n);
            assert_eq!(vf.output.len() + vf.input.len(), FRAME);
        }
    }

    #[test]
    fn removes_steady_background_noise() {
        let mut vf = VoiceFocus::new(48_000).unwrap();
        let mut seed = 7;
        let mut x = noise(48_000 * 3, 0.03, &mut seed); // fan/hiss at about -35 dBFS RMS
        let before = rms(&x[48_000..]);
        vf.process(&mut x);
        let after = rms(&x[48_000 * 2..]);
        let db = 20.0 * (after / before).log10();
        println!("background noise: {db:.1} dB");
        assert!(db < -15.0, "noise only reduced by {db:.1} dB");
    }

    #[test]
    fn keeps_voice_over_noise() {
        let mut vf = VoiceFocus::new(48_000).unwrap();
        let mut seed = 3;
        let n = 48_000 * 4;
        let voice = speechy(n, 0.15);
        let bg = noise(n, 0.02, &mut seed);
        let mut mix: Vec<f32> = voice.iter().zip(&bg).map(|(v, b)| v + b).collect();
        vf.process(&mut mix);
        // After settling, the output should still carry a healthy voice-level signal...
        let out = rms(&mix[48_000 * 2..]);
        println!("voice in {:.3} rms -> out {out:.3} rms", rms(&voice[48_000 * 2..]));
        assert!(out > 0.03, "voice was suppressed: rms {out}");
        // ...and never clip.
        assert!(mix.iter().all(|v| v.abs() <= 1.0));
    }

    #[test]
    fn added_delay_is_about_one_to_two_blocks() {
        let mut vf = VoiceFocus::new(48_000).unwrap();
        let input = speechy(48_000 * 2, 0.1);
        let mut out = input.clone();
        vf.process(&mut out);
        // Find the lag with the best correlation between input and output (second half, settled).
        let (a, b) = (&input[48_000..], &out[48_000..]);
        let best = (0..1500)
            .max_by(|&x, &y| {
                let c = |l: usize| a[..a.len() - 1500].iter().zip(&b[l..]).map(|(p, q)| p * q).sum::<f32>();
                c(x).total_cmp(&c(y))
            })
            .unwrap();
        println!("voice focus delay: {best} samples = {:.1} ms", best as f32 / 48.0);
        assert!(best <= 2 * FRAME, "{best}");
    }

    #[test]
    fn high_pass_cuts_rumble_keeps_voice_band() {
        let gain = |f: f32| {
            let mut hp = HighPass::new(90.0, 48_000.0);
            let y: Vec<f32> = (0..48_000).map(|i| hp.tick((std::f32::consts::TAU * f * i as f32 / 48_000.0).sin())).collect();
            rms(&y[24_000..]) / std::f32::consts::FRAC_1_SQRT_2
        };
        assert!(gain(30.0) < 0.15, "30 Hz {}", gain(30.0));
        assert!(gain(300.0) > 0.95, "300 Hz {}", gain(300.0));
    }

    #[test]
    fn limiter_is_soft_and_bounded() {
        assert_eq!(soft_limit(0.5), 0.5);
        assert!(soft_limit(3.0) <= 1.0 && soft_limit(3.0) > 0.95);
        assert!(soft_limit(-3.0) >= -1.0);
    }
}
