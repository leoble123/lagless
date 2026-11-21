//! Sound awareness: listen to the room through the Mac's mic and flag sounds worth taking your
//! IEMs out for (doorbell, knock, alarms, phone, dog, baby, shouting, breaking glass).
//!
//! mic (any rate) -> anti-aliased resample to 16 kHz -> log-mel rows -> YAMNet every 0.48 s
//! -> per-category score -> "present in two overlapping windows, or very strong once" -> alert,
//! then that category stays quiet for a cooldown so one doorbell is one alert.

use crate::yamnet::{LogMel, Model, HOP, MELS, PATCH_FRAMES, SAMPLE_RATE};
use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Category {
    Doorbell,
    Knock,
    Alarm,
    Phone,
    Dog,
    Cat,
    Baby,
    Shout,
    Breaking,
    Talking,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Doorbell,
        Category::Knock,
        Category::Alarm,
        Category::Phone,
        Category::Dog,
        Category::Baby,
        Category::Shout,
        Category::Breaking,
        Category::Cat,
        Category::Talking,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Doorbell => "Doorbell",
            Category::Knock => "Knocking",
            Category::Alarm => "Alarm",
            Category::Phone => "Phone ringing",
            Category::Dog => "Dog barking",
            Category::Cat => "Cat",
            Category::Baby => "Baby crying",
            Category::Shout => "Someone shouting",
            Category::Breaking => "Something broke",
            Category::Talking => "Someone talking",
        }
    }

    /// Sounds that are over in well under a second (a knock, a bark): a single analysis window
    /// above threshold is enough. Everything else must show up twice in a row or be strong.
    fn impulsive(self) -> bool {
        matches!(self, Category::Knock | Category::Dog | Category::Breaking | Category::Doorbell)
    }

    /// Classes that, when they score higher, mean "it's actually that": a ticking clock can look
    /// a little like a phone, a meow like a crying baby.
    fn rivals(self) -> &'static [usize] {
        match self {
            Category::Phone => &[400, 402], // Clock, Tick-tock
            Category::Baby => &[76, 78, 80], // Cat, Meow, Caterwaul
            _ => &[],
        }
    }

    /// Sounds the model easily mixes up share one cooldown, so one event is one alert.
    fn family(self) -> u8 {
        match self {
            Category::Alarm | Category::Phone | Category::Doorbell => 1,
            Category::Baby | Category::Cat => 2,
            _ => 10 + self as u8,
        }
    }

    /// Off by default: a cat is rarely urgent, and "talking" would fire on your own voice chat.
    pub fn on_by_default(self) -> bool {
        !matches!(self, Category::Cat | Category::Talking)
    }

    /// (YAMNet class indices, base threshold).
    fn classes(self) -> (&'static [usize], f32) {
        match self {
            Category::Doorbell => (&[349, 350], 0.35),
            Category::Knock => (&[353], 0.35),
            // Alarm, Alarm clock, Siren, Civil defense siren, Buzzer, Smoke detector, Fire alarm, Car alarm
            Category::Alarm => (&[382, 389, 390, 391, 392, 393, 394, 304], 0.3),
            Category::Phone => (&[383, 384, 385], 0.3),
            Category::Dog => (&[69, 70, 71, 72, 73, 74, 75], 0.4),
            Category::Cat => (&[76, 78, 80], 0.5),
            Category::Baby => (&[20, 19], 0.35),
            Category::Shout => (&[6, 7, 9, 10, 11], 0.35),
            Category::Breaking => (&[437, 435, 463, 464], 0.35),
            Category::Talking => (&[0, 2], 0.6),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub category: Category,
    /// "Smoke alarm" etc. when the specific class is clearer than the category.
    pub text: String,
    pub score: f32,
    /// Seconds of audio processed when it fired.
    pub at: f64,
}

/// Low-pass + linear interpolation to 16 kHz. Fine for classification, any input rate.
struct To16k {
    taps: Vec<f32>,
    hist: VecDeque<f32>,
    step: f64,
    pos: f64,
    prev: f32,
}

impl To16k {
    fn new(rate: u32) -> Self {
        let n = 47;
        let fc = (7_600.0 / rate as f64).min(0.49);
        let taps: Vec<f32> = (0..n)
            .map(|i| {
                let x = i as f64 - (n - 1) as f64 / 2.0;
                let sinc = if x == 0.0 { 2.0 * fc } else { (std::f64::consts::TAU * fc * x).sin() / (std::f64::consts::PI * x) };
                let w = 0.42 - 0.5 * (std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos() + 0.08 * (2.0 * std::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
                (sinc * w) as f32
            })
            .collect();
        To16k { hist: std::iter::repeat_n(0.0, n).collect(), taps, step: rate as f64 / SAMPLE_RATE as f64, pos: 0.0, prev: 0.0 }
    }

    fn push(&mut self, x: f32, out: &mut Vec<f32>) {
        self.hist.pop_front();
        self.hist.push_back(x);
        let y: f32 = self.hist.iter().zip(&self.taps).map(|(a, b)| a * b).sum();
        // Emit every output instant that falls between the previous and this filtered sample.
        while self.pos <= 1.0 {
            out.push(self.prev + (y - self.prev) * self.pos as f32);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = y;
    }
}

pub struct Detector {
    model: Arc<Model>,
    mel: LogMel,
    resample: To16k,
    wave: Vec<f32>,
    rows: VecDeque<[f32; MELS]>,
    rows_since_patch: usize,
    clock: f64,
    enabled: Vec<Category>,
    /// 0..1; 0.5 = the base thresholds.
    sensitivity: f32,
    prev: Vec<f32>,
    last_alert: Vec<f64>,
    scratch: Vec<f32>,
    /// Decaying max of each category's rival score (meows, ticks come and go between windows).
    recent_rival: Vec<f32>,
    last_any: f64,
}

pub const COOLDOWN_SECS: f64 = 8.0;
/// After any alert, other kinds of sound wait this long: one event, one alert.
const SETTLE_SECS: f64 = 4.0;

impl Detector {
    pub fn new(model: Arc<Model>, input_rate: u32, enabled: Vec<Category>, sensitivity: f32) -> Self {
        let n = Category::ALL.len();
        Detector {
            model,
            mel: LogMel::new(),
            resample: To16k::new(input_rate),
            wave: Vec::with_capacity(4096),
            rows: VecDeque::with_capacity(PATCH_FRAMES + 1),
            rows_since_patch: 0,
            clock: 0.0,
            enabled,
            sensitivity: sensitivity.clamp(0.0, 1.0),
            prev: vec![0.0; n],
            last_alert: vec![f64::NEG_INFINITY; n],
            scratch: Vec::with_capacity(PATCH_FRAMES * MELS),
            recent_rival: vec![0.0; n],
            last_any: f64::NEG_INFINITY,
        }
    }

    /// Feed mono samples at the input rate; returns any alerts that fired.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Alert> {
        let mut alerts = Vec::new();
        let mut buf = Vec::with_capacity(samples.len() / 2 + 2);
        for &s in samples {
            self.resample.push(s, &mut buf);
        }
        self.wave.extend_from_slice(&buf);
        let mut consumed = 0;
        while self.wave.len() - consumed >= 400 {
            let mut row = [0f32; MELS];
            self.mel.frame(&self.wave[consumed..consumed + 400], &mut row);
            consumed += HOP;
            self.clock += HOP as f64 / SAMPLE_RATE as f64;
            if self.rows.len() == PATCH_FRAMES {
                self.rows.pop_front();
            }
            self.rows.push_back(row);
            self.rows_since_patch += 1;
            // A patch every 48 rows (0.48 s) once 96 rows (0.96 s) are available, like YAMNet.
            if self.rows.len() == PATCH_FRAMES && self.rows_since_patch >= 48 {
                self.rows_since_patch = 0;
                alerts.extend(self.classify());
            }
        }
        self.wave.drain(..consumed);
        alerts
    }

    fn classify(&mut self) -> Vec<Alert> {
        self.scratch.clear();
        for r in &self.rows {
            self.scratch.extend_from_slice(r);
        }
        let scores = self.model.infer(&self.scratch);
        let mul = 1.5 - self.sensitivity; // 0 -> x1.5 (fewer alerts), 1 -> x0.5 (more)
        // At most one alert per window: the category furthest over its threshold. (A ringing
        // alarm clock also looks a bit like a phone; you only want to hear about one.)
        let mut pick: Option<(usize, usize, f32, f32)> = None; // (k, class, score, ratio)
        for (k, cat) in Category::ALL.iter().enumerate() {
            let (classes, base) = cat.classes();
            let (best, score) = classes.iter().map(|&c| (c, scores[c])).max_by(|a, b| a.1.total_cmp(&b.1)).unwrap();
            let thr = (base * mul).clamp(0.08, 0.95);
            let prev = std::mem::replace(&mut self.prev[k], score);
            let fire = if cat.impulsive() {
                // Over in well under a second: one window is all it gets.
                score >= thr
            } else {
                (score >= thr && prev >= thr) || score >= (thr + 0.15).min(0.9)
            };
            let rival_now = cat.rivals().iter().map(|&c| scores[c]).fold(0.0, f32::max);
            self.recent_rival[k] = rival_now.max(self.recent_rival[k] * 0.7);
            let fire = fire && score > self.recent_rival[k];
            if !fire || !self.enabled.contains(cat) || self.clock - self.last_alert[k] < COOLDOWN_SECS || self.clock - self.last_any < SETTLE_SECS {
                continue;
            }
            let ratio = score / thr;
            if pick.is_none_or(|p| ratio > p.3) {
                pick = Some((k, best, score, ratio));
            }
        }
        let mut out = Vec::new();
        if let Some((k, best, score, _)) = pick {
            let cat = Category::ALL[k];
            self.last_any = self.clock;
            for (j, other) in Category::ALL.iter().enumerate() {
                if other.family() == cat.family() {
                    self.last_alert[j] = self.clock;
                }
            }
            let text = match (cat, best) {
                (Category::Alarm, 393 | 394) => "Smoke alarm".to_string(),
                (Category::Alarm, 390 | 391) => "Siren".to_string(),
                _ => cat.label().to_string(),
            };
            out.push(Alert { category: cat, text, score, at: self.clock });
        }
        out
    }
}

// ---------------------------------------------------------------- hear-through

/// "Let me hear it": the last few seconds of room sound, mixed into the playback (with the game
/// ducked) for a few seconds after an alert. One per app: there's one mic and one playback.
pub struct HearThrough(std::sync::Mutex<Hear>);

struct Hear {
    buf: VecDeque<f32>,
    rate: u32,
    cursor: f64,
    remaining: f64,
    env: f32,
}

pub static HEAR: HearThrough = HearThrough(std::sync::Mutex::new(Hear { buf: VecDeque::new(), rate: 48_000, cursor: 0.0, remaining: 0.0, env: 0.0 }));

/// How far back playback starts, so you hear the sound itself, not the silence after it.
const REWIND_SECS: f64 = 0.8;
const KEEP_SECS: usize = 3;
/// The game drops to this fraction while the room is mixed in.
const DUCK_TO: f32 = 0.25;

impl HearThrough {
    /// Mic side: keep the most recent few seconds.
    pub fn feed(&self, mono: &[f32], rate: u32) {
        let Ok(mut h) = self.0.lock() else { return };
        if h.rate != rate {
            h.buf.clear();
            h.rate = rate;
        }
        h.buf.extend(mono.iter().copied());
        let cap = rate as usize * KEEP_SECS;
        if h.buf.len() > cap {
            let drop = h.buf.len() - cap;
            h.buf.drain(..drop);
            h.cursor = (h.cursor - drop as f64).max(0.0);
        }
    }

    /// Start (or extend) hearing the room for `secs`.
    pub fn trigger(&self, secs: f64) {
        let Ok(mut h) = self.0.lock() else { return };
        if h.remaining <= 0.0 {
            h.cursor = (h.buf.len() as f64 - REWIND_SECS * h.rate as f64).max(0.0);
        }
        h.remaining = h.remaining.max(secs);
    }

    pub fn active(&self) -> bool {
        self.0.lock().map(|h| h.remaining > 0.0 || h.env > 0.0).unwrap_or(false)
    }

    /// Audio callback side: duck `out` and mix the room in. Never blocks: if the mic thread
    /// holds the lock right now, this callback simply skips mixing.
    pub fn mix(&self, out: &mut [f32], out_ch: usize, out_rate: u32, mic_gain: f32) {
        let Ok(mut h) = self.0.try_lock() else { return };
        if h.remaining <= 0.0 && h.env <= 0.0 {
            return;
        }
        let step = h.rate as f64 / out_rate as f64;
        let dt = 1.0 / out_rate as f64;
        let ramp = 1.0 / (0.04 * out_rate as f32); // 40 ms fades, no clicks
        for frame in out.chunks_exact_mut(out_ch.max(1)) {
            let target = if h.remaining > 0.0 { 1.0 } else { 0.0 };
            h.env = (h.env + (target - h.env).clamp(-ramp, ramp)).clamp(0.0, 1.0);
            h.remaining -= dt;
            let i = h.cursor as usize;
            let mic = if i + 1 < h.buf.len() {
                let f = (h.cursor - i as f64) as f32;
                let v = h.buf[i] + (h.buf[i + 1] - h.buf[i]) * f;
                h.cursor += step;
                v
            } else {
                0.0
            };
            let duck = 1.0 - (1.0 - DUCK_TO) * h.env;
            let add = mic * mic_gain * h.env;
            for s in frame.iter_mut() {
                *s = (*s * duck + add).clamp(-1.0, 1.0);
            }
        }
        if h.remaining <= 0.0 && h.env <= 0.0 {
            h.remaining = 0.0;
        }
    }
}

// ---------------------------------------------------------------- listener (Mac)

#[derive(Clone)]
pub struct ListenOptions {
    pub device: Option<String>,
    pub categories: Vec<Category>,
    pub sensitivity: f32,
    /// Mix the room into playback when something fires.
    pub hear: bool,
}

/// Runs the detector on the mic in a background thread until dropped.
pub struct Listener {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    alerts: Arc<std::sync::Mutex<Vec<Alert>>>,
    status: Arc<std::sync::Mutex<Result<(), String>>>,
}

/// How long you hear the room after an alert.
pub const HEAR_SECS: f64 = 6.0;

impl Listener {
    pub fn start(o: ListenOptions, wake: impl Fn() + Send + 'static) -> Self {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let alerts: Arc<std::sync::Mutex<Vec<Alert>>> = Default::default();
        let status = Arc::new(std::sync::Mutex::new(Ok(())));
        let (s, a, st) = (stop.clone(), alerts.clone(), status.clone());
        let handle = std::thread::Builder::new()
            .name("listener".into())
            .spawn(move || {
                if let Err(e) = listen(&o, &s, &a, &wake) {
                    *st.lock().unwrap() = Err(format!("{e:#}"));
                    wake();
                }
            })
            .ok();
        Listener { stop, handle, alerts, status }
    }

    pub fn take_alerts(&self) -> Vec<Alert> {
        std::mem::take(&mut *self.alerts.lock().unwrap())
    }

    pub fn problem(&self) -> Option<String> {
        self.status.lock().unwrap().clone().err()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn listen(o: &ListenOptions, stop: &std::sync::atomic::AtomicBool, out: &std::sync::Mutex<Vec<Alert>>, wake: &dyn Fn()) -> anyhow::Result<()> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use std::sync::atomic::Ordering::Relaxed;
    anyhow::ensure!(Model::available(), "Sound alerts run on the Mac (where the mic is); this Windows build doesn't include the sound model.");
    let model = Arc::new(Model::bundled()?);
    // Debug/testing: play a WAV file into the detector in real time instead of the mic.
    if let Some(path) = std::env::var_os("LAGLESS_LISTEN_WAV") {
        let (rate, wave) = read_wav(std::path::Path::new(&path))?;
        let mut det = Detector::new(model, rate, o.categories.clone(), o.sensitivity);
        let chunk = rate as usize / 25;
        for c in wave.chunks(chunk).cycle() {
            if stop.load(Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
            HEAR.feed(c, rate);
            let fired = det.push(c);
            if !fired.is_empty() {
                if o.hear {
                    HEAR.trigger(HEAR_SECS);
                }
                out.lock().unwrap().extend(fired);
                wake();
            }
        }
        return Ok(());
    }
    let device = crate::audio::find_device(o.device.as_deref(), false)?;
    let cfg = device.default_input_config()?;
    let (rate, ch) = (cfg.sample_rate().0, cfg.channels() as usize);
    let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(rate as usize * 2);
    let got_audio = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g = got_audio.clone();
    let stream = crate::audio::build_input(&device, &cfg.config(), cfg.sample_format(), move |data| {
        g.store(true, Relaxed);
        for f in data.chunks_exact(ch.max(1)) {
            let _ = prod.push(f[..ch.min(2)].iter().sum::<f32>() / ch.min(2) as f32);
        }
    })?;
    stream.play()?;
    let mut det = Detector::new(model, rate, o.categories.clone(), o.sensitivity);
    let started = std::time::Instant::now();
    let mut buf = Vec::with_capacity(rate as usize);
    while !stop.load(Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(40));
        buf.clear();
        while let Ok(s) = cons.pop() {
            buf.push(s);
        }
        if buf.is_empty() {
            if !got_audio.load(Relaxed) && started.elapsed() > std::time::Duration::from_secs(3) {
                anyhow::bail!("{}", crate::mic_access::silent_mic_message());
            }
            continue;
        }
        HEAR.feed(&buf, rate);
        let fired = det.push(&buf);
        if !fired.is_empty() {
            if o.hear {
                HEAR.trigger(HEAR_SECS);
            }
            out.lock().unwrap().extend(fired);
            wake();
        }
    }
    Ok(())
}

/// 16-bit PCM WAV -> (rate, first channel as f32). For tests and `lagless listen --file`.
pub fn read_wav(path: &std::path::Path) -> anyhow::Result<(u32, Vec<f32>)> {
    let b = std::fs::read(path)?;
    anyhow::ensure!(b.len() > 44 && &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE", "not a WAV file");
    anyhow::ensure!(u16::from_le_bytes([b[34], b[35]]) == 16, "only 16-bit PCM WAV is supported");
    let ch = u16::from_le_bytes([b[22], b[23]]).max(1) as usize;
    let rate = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
    let mut p = 12;
    while b.get(p..p + 4).is_some_and(|id| id != b"data") {
        p += 8 + u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
    }
    let data = b.get(p + 8..).ok_or_else(|| anyhow::anyhow!("no audio data"))?;
    Ok((rate, data.chunks_exact(2 * ch).map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_wav(path: &std::path::Path) -> (u32, Vec<f32>) {
        super::read_wav(path).unwrap()
    }

    #[test]
    fn resampler_keeps_time_and_rejects_aliases() {
        for rate in [44_100u32, 48_000, 96_000] {
            let mut r = To16k::new(rate);
            let mut out = Vec::new();
            // 1 s of a 1 kHz tone (keep) plus a 12 kHz tone (would alias into the band).
            for i in 0..rate {
                let t = i as f32 / rate as f32;
                r.push((std::f32::consts::TAU * 1000.0 * t).sin() + (std::f32::consts::TAU * 12_000.0 * t).sin(), &mut out);
            }
            assert!((out.len() as i64 - 16_000).abs() <= 2, "{rate}: {}", out.len());
            let rms = (out[2000..].iter().map(|v| v * v).sum::<f32>() / (out.len() - 2000) as f32).sqrt();
            assert!((rms - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.08, "{rate}: rms {rms}");
        }
    }

    #[test]
    fn hear_through_ducks_and_mixes_with_rewind() {
        let h = HearThrough(std::sync::Mutex::new(Hear { buf: VecDeque::new(), rate: 48_000, cursor: 0.0, remaining: 0.0, env: 0.0 }));
        // 2 s of room: silence, then a "doorbell" (0.5) in the last 0.5 s.
        let mut room = vec![0.0f32; 72_000];
        room.extend(std::iter::repeat_n(0.5f32, 24_000));
        h.feed(&room, 48_000);
        let mut game = vec![0.8f32; 48_000 * 2];
        h.mix(&mut game, 2, 48_000, 1.0);
        assert!(game.iter().all(|&v| v == 0.8), "idle: untouched");
        h.trigger(1.0);
        let mut game = vec![0.8f32; 48_000 * 2]; // 1 s stereo
        h.mix(&mut game, 2, 48_000, 1.0);
        // Ducked after the fade-in...
        assert!((game[2 * 4_000] - 0.8 * DUCK_TO).abs() < 0.01, "{}", game[8000]);
        // ...and the doorbell from 0.8 s back arrives 0.3 s into playback (0.8 - 0.5).
        let at = |t: f32| game[2 * (t * 48_000.0) as usize];
        assert!(at(0.25) < 0.25, "{}", at(0.25));
        assert!(at(0.4) > 0.6, "{}", at(0.4));
        assert!(h.active());
    }

    #[test]
    #[cfg(not(windows))] // the Windows build has no sound model
    fn silence_raises_nothing() {
        let mut d = Detector::new(Arc::new(Model::bundled().unwrap()), 48_000, Category::ALL.to_vec(), 1.0);
        assert!(d.push(&vec![0.0; 48_000 * 3]).is_empty());
    }

    /// Runs real labelled recordings (e.g. ESC-50) through the full detector at their native
    /// rate. Files are named "<category>__<anything>.wav". Set LAGLESS_SOUND_CLIPS.
    #[test]
    #[ignore]
    #[cfg(not(windows))]
    fn real_recordings() {
        let dir = std::env::var("LAGLESS_SOUND_CLIPS").expect("set LAGLESS_SOUND_CLIPS");
        let model = Arc::new(Model::bundled().unwrap());
        let expect = |label: &str| -> Option<Category> {
            Some(match label {
                "dog" => Category::Dog,
                "crying_baby" => Category::Baby,
                "door_wood_knock" => Category::Knock,
                "clock_alarm" | "siren" => Category::Alarm,
                "glass_breaking" => Category::Breaking,
                "cat" => Category::Cat,
                _ => return None, // negatives: nothing should fire
            })
        };
        let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "wav")).collect();
        files.sort();
        let (mut hits, mut pos, mut false_alerts, mut neg) = (0, 0, 0, 0);
        let t0 = std::time::Instant::now();
        let mut audio_secs = 0.0;
        for f in &files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let label = name.split("__").next().unwrap();
            let (rate, wave) = read_wav(f);
            audio_secs += wave.len() as f64 / rate as f64;
            let mut d = Detector::new(model.clone(), rate, Category::ALL.to_vec(), 0.5);
            let alerts: Vec<Alert> = wave.chunks(512).flat_map(|c| d.push(c)).collect();
            let got: Vec<String> = alerts.iter().map(|a| format!("{} {:.2}", a.text, a.score)).collect();
            match expect(label) {
                Some(cat) => {
                    pos += 1;
                    let ok = alerts.iter().any(|a| a.category == cat);
                    hits += ok as usize;
                    let wrong: Vec<_> = alerts.iter().filter(|a| a.category != cat && a.category != Category::Talking).collect();
                    false_alerts += wrong.len();
                    println!("{} {name:44} -> {got:?}", if ok { "OK  " } else { "MISS" });
                }
                None => {
                    neg += 1;
                    let wrong: Vec<_> = alerts.iter().filter(|a| a.category != Category::Talking).collect();
                    false_alerts += wrong.len();
                    println!("{} {name:44} -> {got:?}", if wrong.is_empty() { "ok  " } else { "FALSE" });
                }
            }
        }
        let cpu = t0.elapsed().as_secs_f64();
        println!("detected {hits}/{pos} target sounds; {false_alerts} wrong alerts across {} clips ({neg} negatives)", files.len());
        println!("speed: {audio_secs:.0} s of audio in {cpu:.2} s ({:.1}% of one core)", cpu / audio_secs * 100.0);
        assert!(hits * 100 >= pos * 80, "missed too many");
        assert!(false_alerts * 10 <= files.len(), "too many false alerts");
    }
}
