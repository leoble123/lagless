//! Receiver playback engine, run inside the audio device callback.
//!
//! * Adaptive buffer: as small as the link allows. It is sized from measured packet arrival
//!   gaps, grows on real underruns, and shrinks again when the link is calm.
//! * Clock-drift tracking: the PC's and Mac's sound cards never run at exactly the same rate.
//!   Instead of letting the buffer slowly drain or bloat (the classic "gets laggier, then cuts
//!   out" failure), playback speed is nudged by a few hundredths of a percent through a 32-tap
//!   windowed-sinc resampler that is flat to ~94% of Nyquist (≈22.5 kHz at 48k). It also handles
//!   a real rate mismatch (e.g. 44.1k -> 48k) with proper anti-aliasing.
//! * Pauses on the PC side (nothing playing) are not mistaken for network trouble.

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::Arc;

#[derive(Default)]
pub struct Stats {
    pub target_us: AtomicU32,
    pub fill_us: AtomicU32,
    pub device_us: AtomicU32,
    pub underruns: AtomicU32,
    pub drops: AtomicU32,
    /// Written by the network thread: recent worst-case gap between packets, and frame length.
    pub arrival_gap_us: AtomicU32,
    pub frame_us: AtomicU32,
    /// Set by the network thread when a frame flagged "source resumed after silence" arrives.
    pub discont: AtomicBool,
    pub playing: AtomicBool,
}

const TAPS: usize = 32;
const PHASES: usize = 512;

/// Windowed-sinc fractional-delay filter bank.
struct Sinc {
    table: Vec<[f32; TAPS]>,
}

impl Sinc {
    fn new(cutoff: f64) -> Self {
        let beta = 9.0; // Kaiser window: ~-90 dB stopband
        let half = TAPS as f64 / 2.0 + 0.5;
        let i0b = bessel_i0(beta);
        let table = (0..=PHASES)
            .map(|p| {
                let t = p as f64 / PHASES as f64;
                let mut taps = [0f32; TAPS];
                let mut sum = 0.0;
                let mut raw = [0f64; TAPS];
                for (k, r) in raw.iter_mut().enumerate() {
                    let x = k as f64 - (TAPS / 2 - 1) as f64 - t;
                    let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * cutoff * x) };
                    let w = bessel_i0(beta * (1.0 - (x / half).powi(2)).max(0.0).sqrt()) / i0b;
                    *r = cutoff * sinc * w;
                    sum += *r;
                }
                for (o, r) in taps.iter_mut().zip(raw) {
                    *o = (r / sum) as f32; // unity DC gain at every phase
                }
                taps
            })
            .collect();
        Sinc { table }
    }

    /// `x` = last TAPS input samples (oldest first); output is at position TAPS/2-1+t.
    #[inline]
    fn interp(&self, x: &[f32], t: f64) -> f32 {
        let pf = t * PHASES as f64;
        let p = (pf as usize).min(PHASES - 1);
        let w = (pf - p as f64) as f32;
        let (a, b) = (&self.table[p], &self.table[p + 1]);
        let mut acc = 0.0f32;
        for k in 0..TAPS {
            acc += x[k] * (a[k] + (b[k] - a[k]) * w);
        }
        acc
    }
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..50 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

pub struct Playout {
    cons: Consumer<f32>,
    ch: usize,
    out_ch: usize,
    src_rate: f64,
    dst_rate: f64,
    base_step: f64,
    sinc: Sinc,
    /// Per-channel history, stored twice so the last TAPS samples are always contiguous.
    hist: [[f32; TAPS * 2]; 2],
    hpos: usize,
    frac: f64,
    playing: bool,
    underrun_pending: bool,
    gain: f32,
    gain_step: f32,
    tail: [f32; 2],
    target: f64,
    user_min: f64,
    max_target: f64,
    fill_avg: f64,
    integ: f64,
    calm_secs: f64,
    stats: Arc<Stats>,
}

impl Playout {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cons: Consumer<f32>,
        ch: usize,
        out_ch: usize,
        src_rate: u32,
        dst_rate: u32,
        min_ms: f64,
        max_ms: f64,
        stats: Arc<Stats>,
    ) -> Self {
        let src = src_rate as f64;
        let base_step = src / dst_rate as f64;
        Playout {
            cons,
            ch,
            out_ch,
            src_rate: src,
            dst_rate: dst_rate as f64,
            base_step,
            // When downsampling, lower the cutoff below the *output* Nyquist to prevent aliasing.
            sinc: Sinc::new(0.94 * (1.0 / base_step).min(1.0)),
            hist: [[0.0; TAPS * 2]; 2],
            hpos: 0,
            frac: 0.0,
            playing: false,
            underrun_pending: false,
            gain: 0.0,
            gain_step: 1.0 / (0.002 * dst_rate as f64) as f32,
            tail: [0.0; 2],
            target: min_ms * src / 1000.0,
            user_min: min_ms * src / 1000.0,
            max_target: max_ms * src / 1000.0,
            fill_avg: 0.0,
            integ: 0.0,
            calm_secs: 0.0,
            stats,
        }
    }

    fn us(&self, frames: f64) -> u32 {
        (frames / self.src_rate * 1e6) as u32
    }

    /// (minimum buffer target, recent worst arrival gap), both in source frames.
    fn floor(&self, cb_frames: usize) -> (f64, f64) {
        let frame = self.stats.frame_us.load(Relaxed) as f64 * 1e-6 * self.src_rate;
        let gap = self.stats.arrival_gap_us.load(Relaxed) as f64 * 1e-6 * self.src_rate;
        // Must ride out one device callback, a frame or two, and the burstiest recent arrivals
        // (the fill level swings ±gap/2 around its average).
        let need = cb_frames as f64 * self.base_step + frame * 2.0 + gap * 0.6;
        (self.user_min.max(need), gap)
    }

    /// Fill `out` (interleaved, `out_ch` channels, device rate).
    pub fn process(&mut self, out: &mut [f32]) {
        let frames = out.len() / self.out_ch;
        let cb_secs = frames as f64 / self.dst_rate;

        let (min_target, gap) = self.floor(frames);
        self.target = self.target.clamp(min_target, self.max_target.max(min_target));

        let mut fill = (self.cons.slots() / self.ch) as f64;
        self.stats.fill_us.store(self.us(fill), Relaxed);
        self.stats.target_us.store(self.us(self.target), Relaxed);

        if !self.playing {
            // Starting right on a burst peak would leave the average half a gap short.
            if fill < self.target + gap * 0.5 {
                self.write_tail(out, 0);
                return;
            }
            self.start_playing(fill);
        }

        // A burst after a Wi-Fi stall: skip ahead instead of carrying the extra latency forever.
        let excess_limit = self.target + (0.03 * self.src_rate).max(self.target);
        if fill > excess_limit {
            let drop = (fill - self.target) as usize * self.ch;
            if let Ok(chunk) = self.cons.read_chunk(drop) {
                chunk.commit_all();
            }
            fill = self.target;
            self.fill_avg = fill;
            self.stats.drops.fetch_add(1, Relaxed);
        }

        // PI controller on the smoothed fill level -> tiny playback-rate correction (max ±0.5%).
        let alpha = (cb_secs / 0.5).min(1.0);
        self.fill_avg += (fill - self.fill_avg) * alpha;
        let err = (self.fill_avg - self.target) / self.src_rate;
        self.integ = (self.integ + err * cb_secs).clamp(-0.02, 0.02);
        let adj = (0.5 * err + 0.05 * self.integ).clamp(-0.005, 0.005);
        let step = self.base_step * (1.0 + adj);

        for i in 0..frames {
            while self.frac >= 1.0 {
                if !self.pull() {
                    self.underrun(out, i);
                    return;
                }
                self.frac -= 1.0;
            }
            let mut y = [0.0f32; 2];
            for (c, yc) in y.iter_mut().enumerate().take(self.ch) {
                let h = &self.hist[c][self.hpos..self.hpos + TAPS];
                *yc = self.sinc.interp(h, self.frac) * self.gain;
            }
            if self.gain < 1.0 {
                self.gain = (self.gain + self.gain_step).min(1.0);
            }
            self.tail = y;
            let base = i * self.out_ch;
            write_frame(&mut out[base..base + self.out_ch], &y, self.ch);
            self.frac += step;
        }

        self.calm_secs += cb_secs;
        if self.calm_secs > 15.0 {
            self.calm_secs = 0.0;
            self.target = (self.target * 0.85).max(min_target);
        }
    }

    fn start_playing(&mut self, fill: f64) {
        self.playing = true;
        self.stats.playing.store(true, Relaxed);
        self.gain = 0.0;
        self.hist = [[0.0; TAPS * 2]; 2];
        self.frac = 0.0;
        self.fill_avg = fill;
        self.integ = 0.0;
        if self.underrun_pending {
            self.underrun_pending = false;
            // If the sender told us it had paused, the gap wasn't the network's fault.
            if !self.stats.discont.swap(false, Relaxed) {
                self.stats.underruns.fetch_add(1, Relaxed);
                self.target = (self.target * 1.5 + 0.001 * self.src_rate).min(self.max_target);
            }
        }
    }

    fn underrun(&mut self, out: &mut [f32], from_frame: usize) {
        self.playing = false;
        self.stats.playing.store(false, Relaxed);
        self.underrun_pending = true;
        self.stats.discont.store(false, Relaxed);
        self.calm_secs = 0.0;
        self.write_tail(out, from_frame);
    }

    /// Fade whatever was last played to silence instead of clicking.
    fn write_tail(&mut self, out: &mut [f32], from_frame: usize) {
        for f in out[from_frame * self.out_ch..].chunks_exact_mut(self.out_ch) {
            for t in self.tail.iter_mut() {
                *t *= 0.995;
            }
            write_frame(f, &self.tail, self.ch);
        }
    }

    fn pull(&mut self) -> bool {
        if self.cons.slots() < self.ch {
            return false;
        }
        for c in 0..self.ch {
            let s = self.cons.pop().unwrap_or(0.0);
            self.hist[c][self.hpos] = s;
            self.hist[c][self.hpos + TAPS] = s;
        }
        self.hpos = (self.hpos + 1) % TAPS;
        true
    }
}

fn write_frame(out: &mut [f32], y: &[f32; 2], src_ch: usize) {
    match (src_ch, out.len()) {
        (1, _) => out.fill(y[0]),
        (_, 1) => out[0] = 0.5 * (y[0] + y[1]),
        _ => {
            out[0] = y[0];
            out[1] = y[1];
            out[2..].fill(0.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    fn stats(frame_us: u32, gap_us: u32) -> Arc<Stats> {
        let s = Arc::new(Stats::default());
        s.frame_us.store(frame_us, Relaxed);
        s.arrival_gap_us.store(gap_us, Relaxed);
        s
    }

    /// Simulate a sender whose clock runs `ppm` fast/slow relative to the output device, with
    /// packets arriving in WASAPI-style 10 ms bursts, and check the buffer settles at its target
    /// with no glitches instead of drifting into an underrun or overflow.
    fn drift(ppm: f64) -> (f64, u32, u32) {
        let st = stats(1000, 10_000);
        let (mut prod, cons) = RingBuffer::new(48000 * 2);
        let mut p = Playout::new(cons, 2, 2, 48000, 48000, 2.0, 150.0, st.clone());
        let mut out = vec![0f32; 64 * 2];
        let src_per_cb = 64.0 * (1.0 + ppm / 1e6);
        let mut owed = 0.0;
        let mut phase = 0.0f32;
        // 10 minutes of audio
        for _ in 0..(600 * 48000 / 64) {
            owed += src_per_cb;
            while owed >= 480.0 {
                owed -= 480.0;
                for _ in 0..480 {
                    phase += 0.01;
                    let s = phase.sin() * 0.5;
                    let _ = prod.push(s);
                    let _ = prod.push(s);
                }
            }
            p.process(&mut out);
        }
        let fill_ms = st.fill_us.load(Relaxed) as f64 / 1000.0;
        (fill_ms, st.underruns.load(Relaxed), st.drops.load(Relaxed))
    }

    #[test]
    fn tracks_clock_drift_both_ways() {
        for ppm in [-300.0, -50.0, 0.0, 50.0, 300.0] {
            let (fill_ms, underruns, drops) = drift(ppm);
            assert_eq!((underruns, drops), (0, 0), "ppm {ppm}");
            assert!(fill_ms < 20.0, "ppm {ppm}: fill {fill_ms}ms");
        }
    }

    #[test]
    fn sinc_is_transparent() {
        let s = Sinc::new(0.94);
        // Passband: a 20 kHz tone at 48k, interpolated at an awkward fractional position.
        let f = 20_000.0 / 48_000.0;
        let x: Vec<f32> = (0..TAPS).map(|n| (2.0 * std::f64::consts::PI * f * n as f64).sin() as f32).collect();
        for t in [0.0, 0.3, 0.5, 0.77] {
            let pos = (TAPS / 2 - 1) as f64 + t;
            let want = (2.0 * std::f64::consts::PI * f * pos).sin() as f32;
            let got = s.interp(&x, t);
            assert!((got - want).abs() < 0.03, "t {t}: {got} vs {want}");
        }
        // DC passes at unity.
        assert!((s.interp(&[1.0; TAPS], 0.42) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn underrun_grows_target_but_pause_does_not() {
        let st = stats(1000, 1000);
        let (mut prod, cons) = RingBuffer::new(48000);
        let mut p = Playout::new(cons, 1, 2, 48000, 48000, 2.0, 150.0, st.clone());
        let mut out = vec![0f32; 64 * 2];
        let feed = |prod: &mut rtrb::Producer<f32>, n| (0..n).for_each(|_| { let _ = prod.push(0.1); });

        feed(&mut prod, 960);
        for _ in 0..40 {
            p.process(&mut out);
        }
        let t0 = p.target;
        // Network glitch: resume without the discont flag.
        feed(&mut prod, 4800);
        p.process(&mut out);
        assert_eq!(st.underruns.load(Relaxed), 1);
        assert!(p.target > t0);

        // Sender paused: resume flagged as discontinuity.
        for _ in 0..120 {
            p.process(&mut out);
        }
        let t1 = p.target;
        st.discont.store(true, Relaxed);
        feed(&mut prod, 4800);
        p.process(&mut out);
        assert_eq!(st.underruns.load(Relaxed), 1);
        assert_eq!(p.target, t1);
    }
}
