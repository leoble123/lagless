//! YAMNet sound classifier (Google, Apache-2.0): 521 AudioSet classes from 0.96 s of 16 kHz audio.
//!
//! A from-scratch port of the published model (tensorflow/models research/audioset/yamnet):
//! log-mel front end, MobileNet-v1 body, sigmoid classifier. Weights are the official yamnet.h5,
//! converted by tools/yamnet_convert.py with batch-norm folded into each conv and stored as f16.
//! Checked against TensorFlow running the same weights (see the `matches_tensorflow` test).

use rustfft::{num_complex::Complex, Fft, FftPlanner};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 16_000;
pub const CLASSES: usize = 521;
const WIN: usize = 400; // 25 ms
pub const HOP: usize = 160; // 10 ms
const NFFT: usize = 512;
const BINS: usize = NFFT / 2 + 1;
pub const MELS: usize = 64;
pub const PATCH_FRAMES: usize = 96; // 0.96 s
const EMBED: usize = 1024;

/// (stride, output channels) of the depthwise-separable blocks after the first conv.
const BLOCKS: [(usize, usize); 13] =
    [(1, 64), (2, 128), (1, 128), (2, 256), (1, 256), (2, 512), (1, 512), (1, 512), (1, 512), (1, 512), (1, 512), (2, 1024), (1, 1024)];

/// Short-time log-mel spectrogram, exactly as YAMNet's features.py (tf.signal.stft with a periodic
/// Hann window, magnitude, tf.signal.linear_to_mel_weight_matrix, log(x + 0.001)).
pub struct LogMel {
    fft: Arc<dyn Fft<f32>>,
    window: [f32; WIN],
    mel: Vec<[f32; MELS]>, // [BINS][MELS]
    buf: Vec<Complex<f32>>,
}

impl Default for LogMel {
    fn default() -> Self {
        Self::new()
    }
}

impl LogMel {
    pub fn new() -> Self {
        let mut window = [0f32; WIN];
        for (i, w) in window.iter_mut().enumerate() {
            *w = (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / WIN as f64).cos()) as f32;
        }
        LogMel { fft: FftPlanner::new().plan_fft_forward(NFFT), window, mel: mel_matrix(), buf: vec![Complex::default(); NFFT] }
    }

    /// One analysis frame: `samples` must hold WIN (400) samples at 16 kHz.
    pub fn frame(&mut self, samples: &[f32], out: &mut [f32; MELS]) {
        for (i, c) in self.buf.iter_mut().enumerate() {
            *c = Complex::new(if i < WIN { samples[i] * self.window[i] } else { 0.0 }, 0.0);
        }
        self.fft.process(&mut self.buf);
        let mut acc = [0f32; MELS];
        for (b, w) in self.mel.iter().enumerate() {
            let mag = self.buf[b].norm();
            if mag != 0.0 {
                for m in 0..MELS {
                    acc[m] += mag * w[m];
                }
            }
        }
        for m in 0..MELS {
            out[m] = (acc[m] + 0.001).ln();
        }
    }
}

/// tf.signal.linear_to_mel_weight_matrix(64, 257, 16000, 125, 7500): HTK mel, DC row zero.
fn mel_matrix() -> Vec<[f32; MELS]> {
    let hz_to_mel = |hz: f64| 1127.0 * (1.0 + hz / 700.0).ln();
    let (lo, hi) = (hz_to_mel(125.0), hz_to_mel(7500.0));
    let edges: Vec<f64> = (0..MELS + 2).map(|i| lo + (hi - lo) * i as f64 / (MELS + 1) as f64).collect();
    let nyquist = SAMPLE_RATE as f64 / 2.0;
    let mut m = vec![[0f32; MELS]; BINS];
    for (b, row) in m.iter_mut().enumerate().skip(1) {
        let mel = hz_to_mel(nyquist * b as f64 / (BINS - 1) as f64);
        for j in 0..MELS {
            let (l, c, u) = (edges[j], edges[j + 1], edges[j + 2]);
            let w = ((mel - l) / (c - l)).min((u - mel) / (u - c));
            row[j] = w.max(0.0) as f32;
        }
    }
    m
}

struct Layer {
    w: Vec<f32>,
    b: Vec<f32>,
}

pub struct Model {
    first: Layer,
    blocks: Vec<(Layer, Layer)>, // (depthwise, pointwise)
    dense: Layer,
}

/// Tensors are HWC: index (y * w + x) * c + ch.
struct Tensor {
    h: usize,
    w: usize,
    c: usize,
    d: Vec<f32>,
}

/// TF "same" padding: output ceil(n / stride), extra padding on the bottom/right.
fn same(n: usize, stride: usize) -> (usize, usize) {
    let out = n.div_ceil(stride);
    let pad = ((out - 1) * stride + 3).saturating_sub(n);
    (out, pad / 2)
}

impl Model {
    #[cfg_attr(windows, allow(dead_code))]
    pub fn load(blob: &[u8]) -> Result<Self, &'static str> {
        if blob.len() < 8 || &blob[0..4] != b"YAMN" {
            return Err("not a YAMNet weights blob");
        }
        let mut p = 8;
        let mut take = |nw: usize, nb: usize| -> Result<Layer, &'static str> {
            let need = nw * 2 + nb * 4;
            let s = blob.get(p..p + need).ok_or("weights blob too short")?;
            p += need;
            let w = s[..nw * 2].chunks_exact(2).map(|c| f16_to_f32(u16::from_le_bytes([c[0], c[1]]))).collect();
            let b = s[nw * 2..].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            Ok(Layer { w, b })
        };
        let first = take(32 * 9, 32)?;
        let mut blocks = Vec::new();
        let mut cin = 32;
        for &(_, cout) in &BLOCKS {
            let dw = take(cin * 9, cin)?;
            let pw = take(cout * cin, cout)?;
            blocks.push((dw, pw));
            cin = cout;
        }
        let dense = take(CLASSES * EMBED, CLASSES)?;
        Ok(Model { first, blocks, dense })
    }

    /// The official weights. On macOS they ship once in Lagless.app/Contents/Resources: the app
    /// is a universal binary, so embedding them would carry two copies. Not in the Windows build
    /// at all: detection runs where the mic is (the Mac). Elsewhere (dev/tests) they're embedded.
    #[cfg(target_os = "macos")]
    pub fn bundled() -> anyhow::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut tried = Vec::new();
        for p in [
            exe.parent().and_then(|d| d.parent()).map(|c| c.join("Resources/yamnet.bin")),
            Some(std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/yamnet.bin"))),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(b) = std::fs::read(&p) {
                return Self::load(&b).map_err(|e| anyhow::anyhow!(e));
            }
            tried.push(p.display().to_string());
        }
        anyhow::bail!("sound model not found (looked in {})", tried.join(", "))
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn bundled() -> anyhow::Result<Self> {
        Self::load(include_bytes!("../assets/yamnet.bin")).map_err(|e| anyhow::anyhow!(e))
    }

    #[cfg(windows)]
    pub fn bundled() -> anyhow::Result<Self> {
        anyhow::bail!("the sound model isn't included in the Windows build")
    }

    pub fn available() -> bool {
        cfg!(not(windows))
    }

    /// Scores (0..1, independent sigmoids) for one 96x64 log-mel patch, row-major [frame][mel].
    pub fn infer(&self, patch: &[f32]) -> Vec<f32> {
        assert_eq!(patch.len(), PATCH_FRAMES * MELS);
        // First layer: 3x3 conv, stride 2, 1 -> 32 channels.
        let (oh, pt) = same(PATCH_FRAMES, 2);
        let (ow, pl) = same(MELS, 2);
        let mut t = Tensor { h: oh, w: ow, c: 32, d: vec![0.0; oh * ow * 32] };
        for y in 0..oh {
            for x in 0..ow {
                for co in 0..32 {
                    let mut acc = self.first.b[co];
                    for ky in 0..3 {
                        let iy = (y * 2 + ky) as isize - pt as isize;
                        if iy < 0 || iy >= PATCH_FRAMES as isize {
                            continue;
                        }
                        for kx in 0..3 {
                            let ix = (x * 2 + kx) as isize - pl as isize;
                            if ix < 0 || ix >= MELS as isize {
                                continue;
                            }
                            acc += patch[iy as usize * MELS + ix as usize] * self.first.w[co * 9 + ky * 3 + kx];
                        }
                    }
                    t.d[(y * ow + x) * 32 + co] = acc.max(0.0);
                }
            }
        }
        for ((stride, cout), (dw, pw)) in BLOCKS.iter().zip(&self.blocks) {
            t = depthwise(&t, dw, *stride);
            t = pointwise(&t, pw, *cout);
        }
        // Global average pool -> dense -> sigmoid.
        let n = (t.h * t.w) as f32;
        let mut emb = vec![0f32; EMBED];
        for px in t.d.chunks_exact(EMBED) {
            for (e, v) in emb.iter_mut().zip(px) {
                *e += v / n;
            }
        }
        (0..CLASSES)
            .map(|k| {
                let logit = self.dense.b[k] + dot(&self.dense.w[k * EMBED..(k + 1) * EMBED], &emb);
                1.0 / (1.0 + (-logit).exp())
            })
            .collect()
    }
}

fn depthwise(t: &Tensor, l: &Layer, stride: usize) -> Tensor {
    let (oh, pt) = same(t.h, stride);
    let (ow, pl) = same(t.w, stride);
    let c = t.c;
    let mut out = Tensor { h: oh, w: ow, c, d: vec![0.0; oh * ow * c] };
    for y in 0..oh {
        for x in 0..ow {
            let o = &mut out.d[(y * ow + x) * c..(y * ow + x + 1) * c];
            o.copy_from_slice(&l.b);
            for ky in 0..3 {
                let iy = (y * stride + ky) as isize - pt as isize;
                if iy < 0 || iy >= t.h as isize {
                    continue;
                }
                for kx in 0..3 {
                    let ix = (x * stride + kx) as isize - pl as isize;
                    if ix < 0 || ix >= t.w as isize {
                        continue;
                    }
                    let i = &t.d[(iy as usize * t.w + ix as usize) * c..][..c];
                    let k = ky * 3 + kx;
                    for ch in 0..c {
                        o[ch] += i[ch] * l.w[ch * 9 + k];
                    }
                }
            }
            for v in o.iter_mut() {
                *v = v.max(0.0);
            }
        }
    }
    out
}

fn pointwise(t: &Tensor, l: &Layer, cout: usize) -> Tensor {
    let cin = t.c;
    let mut out = Tensor { h: t.h, w: t.w, c: cout, d: vec![0.0; t.h * t.w * cout] };
    for (px, o) in t.d.chunks_exact(cin).zip(out.d.chunks_exact_mut(cout)) {
        for (co, v) in o.iter_mut().enumerate() {
            *v = (l.b[co] + dot(&l.w[co * cin..(co + 1) * cin], px)).max(0.0);
        }
    }
    out
}

/// Dot product in 8 independent lanes so the compiler can vectorise it.
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 8];
    let (ac, bc) = (a.chunks_exact(8), b.chunks_exact(8));
    let (ar, br) = (ac.remainder(), bc.remainder());
    for (x, y) in ac.zip(bc) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum::<f32>() + ar.iter().zip(br).map(|(x, y)| x * y).sum::<f32>()
}

fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if frac == 0 => sign,
        0 => {
            // subnormal: renormalise
            let mut e = 127 - 15 + 1;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        31 => sign | 0x7f80_0000 | (frac << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

/// Display names in class-index order (the official yamnet_class_map.csv).
#[cfg(test)]
pub fn class_names() -> Vec<String> {
    include_str!("../assets/yamnet_class_map.csv")
        .lines()
        .skip(1)
        .map(|l| {
            // index,mid,display_name (name may be quoted and contain commas)
            let rest = l.splitn(3, ',').nth(2).unwrap_or("");
            rest.trim().trim_matches('"').to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_floats() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!((f16_to_f32(0x0001) - 5.96e-8).abs() < 1e-9); // smallest subnormal
        assert!((f16_to_f32(0x3555) - 0.33325).abs() < 1e-4);
    }

    #[test]
    fn class_map_loads() {
        let n = class_names();
        assert_eq!(n.len(), CLASSES);
        assert_eq!(n[0], "Speech");
        assert!(n.iter().any(|s| s == "Doorbell"));
    }

    #[test]
    fn same_padding_matches_tf() {
        assert_eq!(same(96, 2), (48, 0));
        assert_eq!(same(64, 1), (64, 1));
        assert_eq!(same(3, 2), (2, 1));
    }

    /// Compares against reference outputs from TensorFlow running the official weights.
    /// Set LAGLESS_YAMNET_REF to a folder of <clip>.pcm (16 kHz f32), .mel and .scores files.
    #[test]
    #[ignore]
    #[cfg(not(windows))]
    fn matches_tensorflow() {
        let dir = std::env::var("LAGLESS_YAMNET_REF").expect("set LAGLESS_YAMNET_REF");
        let model = Model::bundled().unwrap();
        let mut mel = LogMel::new();
        let read = |p: std::path::PathBuf| -> Vec<f32> {
            std::fs::read(p).unwrap().chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
        };
        let (mut worst_mel, mut worst_score, mut top_agree, mut n) = (0f32, 0f32, 0, 0);
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "pcm")).collect();
        files.sort();
        for pcm in files {
            let wave = read(pcm.clone());
            let ref_mel = read(pcm.with_extension("mel"));
            let ref_scores = read(pcm.with_extension("scores"));
            let frames = ref_mel.len() / MELS;
            let mut rows = vec![0f32; frames * MELS];
            for f in 0..frames {
                let mut r = [0f32; MELS];
                mel.frame(&wave[f * HOP..f * HOP + WIN], &mut r);
                rows[f * MELS..(f + 1) * MELS].copy_from_slice(&r);
            }
            for (a, b) in rows.iter().zip(&ref_mel) {
                worst_mel = worst_mel.max((a - b).abs());
            }
            let patches = ref_scores.len() / CLASSES;
            for p in 0..patches {
                let got = model.infer(&ref_mel[p * 48 * MELS..(p * 48 + PATCH_FRAMES) * MELS]);
                let want = &ref_scores[p * CLASSES..(p + 1) * CLASSES];
                for (a, b) in got.iter().zip(want) {
                    worst_score = worst_score.max((a - b).abs());
                }
                let top = |v: &[f32]| (0..CLASSES).max_by(|&i, &j| v[i].total_cmp(&v[j])).unwrap();
                top_agree += (top(&got) == top(want)) as usize;
                n += 1;
            }
        }
        println!("patches {n}, top-1 agreement {top_agree}/{n}, worst |mel diff| {worst_mel:.4}, worst |score diff| {worst_score:.4}");
        assert!(worst_mel < 0.02, "front end differs from TF");
        assert!(worst_score < 0.03, "model differs from TF");
        assert!(top_agree * 100 >= n * 98);
    }
}
