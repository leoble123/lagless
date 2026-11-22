//! Sender (the PC): capture -> 1 ms frames -> UDP over the fastest available link.

use crate::audio;
use crate::codec::Encoder;
use crate::engine::{LinkKind, PeerStatus, Phase, Shared};
use crate::net;
use crate::protocol::{self, AudioHeader, Codec, Info, Report, FLAG_DISCONT, FLAG_REDUNDANT, MAX_PACKET};
use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::RingBuffer;
use std::collections::{HashMap, VecDeque};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Serialize, serde::Deserialize)]
pub enum CodecChoice {
    /// 24-bit PCM; drops to Opus by itself only if the link is losing packets
    Auto,
    Pcm24,
    Pcm16,
    Opus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Source {
    /// Whatever the PC is playing (Windows WASAPI loopback)
    Loopback,
    /// A microphone / line-in / virtual cable
    Input,
    /// Test tone, for checking the link without any other audio
    Tone,
}

#[derive(Clone)]
pub struct Options {
    pub to: Option<String>,
    /// Receiver port to stream to: AUDIO_PORT for speakers, MIC_PORT for the mic link.
    pub port: u16,
    pub name: Option<String>,
    pub codec: CodecChoice,
    pub frame_ms: f64,
    pub redundancy: usize,
    pub bitrate_kbps: u32,
    pub source: Source,
    pub device: Option<String>,
    /// Debug: drop this percentage of outgoing datagrams.
    pub sim_loss: f64,
    /// Also send every packet over the second-best link (first copy to arrive wins).
    pub multipath: bool,
    /// Windows: run the audio engine at its minimum period while capturing (see boost.rs).
    pub engine_boost: bool,
    /// Mic link: mono, noise-suppressed, leveled speech (see voice.rs).
    pub voice_focus: bool,
    /// Receiver names not to stream to.
    pub exclude: Vec<String>,
    /// Make every device play at the same delay (the slowest one's), e.g. speakers in one room.
    pub sync: bool,
    /// Windows: the PC's volume dial/keys/mute also set the stream's level.
    pub follow_system_volume: bool,
    pub tone_rate: u32,
}

struct Capture {
    _stream: Option<cpal::Stream>,
    cons: rtrb::Consumer<f32>,
    rate: u32,
    channels: usize,
    /// Largest chunk the OS has handed us, in frames: the capture-side latency floor.
    chunk: Arc<AtomicU32>,
    name: String,
}

fn open_capture(o: &Options) -> Result<Capture> {
    let waker = std::thread::current();
    let chunk = Arc::new(AtomicU32::new(0));
    if o.source == Source::Tone {
        let (rate, channels) = (o.tone_rate, 2usize);
        let (mut prod, cons) = RingBuffer::new(rate as usize * channels);
        let ch = chunk.clone();
        std::thread::spawn(move || {
            // A440 with a gentle beep pattern, generated in real time in 1 ms blocks.
            let start = Instant::now();
            let mut n: u64 = 0;
            loop {
                let due = (start.elapsed().as_secs_f64() * rate as f64) as u64;
                while n < due {
                    let t = n as f64 / rate as f64;
                    let on = (t * 2.0).fract() < 0.5;
                    let s = if on { (t * 440.0 * std::f64::consts::TAU).sin() as f32 * 0.3 } else { 0.0 };
                    let _ = prod.push(s);
                    let _ = prod.push(s);
                    n += 1;
                }
                ch.store(rate / 1000, Relaxed);
                waker.unpark();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        return Ok(Capture { _stream: None, cons, rate, channels, chunk, name: "Test tone".into() });
    }

    let loopback = o.source == Source::Loopback;
    let device = audio::find_device(o.device.as_deref(), loopback)?;
    let cfg = if loopback {
        device.default_output_config()?
    } else {
        let def = device.default_input_config()?;
        // Voice focus runs at 48 kHz; most mics offer it even when their default is 44.1k.
        let at_48k = (o.voice_focus && def.sample_rate().0 != 48_000)
            .then(|| device.supported_input_configs().ok())
            .flatten()
            .and_then(|mut cs| {
                cs.find(|c| c.channels() == def.channels() && c.sample_format() == def.sample_format() && (c.min_sample_rate().0..=c.max_sample_rate().0).contains(&48_000))
            })
            .map(|c| c.with_sample_rate(cpal::SampleRate(48_000)));
        at_48k.unwrap_or(def)
    };
    let (rate, channels) = (cfg.sample_rate().0, cfg.channels() as usize);
    let (mut prod, cons) = RingBuffer::new(rate as usize * channels);
    let ch = chunk.clone();
    let stream = audio::build_input(&device, &cfg.config(), cfg.sample_format(), move |data| {
        ch.fetch_max((data.len() / channels) as u32, Relaxed);
        for &s in data {
            if prod.push(s).is_err() {
                break;
            }
        }
        waker.unpark();
    })
    .with_context(|| {
        if loopback && !cfg!(windows) {
            "loopback capture of the system output only works on Windows; use --source input with a monitor/virtual-cable device".to_string()
        } else {
            format!("capturing from {:?}", device.name().unwrap_or_default())
        }
    })?;
    stream.play()?;
    Ok(Capture { _stream: Some(stream), cons, rate, channels, chunk, name: device.name().unwrap_or_default() })
}

#[derive(Default)]
struct Path {
    rtts: VecDeque<f64>,
    last_pong: Option<Instant>,
}

impl Path {
    fn score(&self) -> Option<f64> {
        let alive = self.last_pong.is_some_and(|t| t.elapsed() < Duration::from_secs(3));
        if alive { self.rtts.iter().copied().reduce(f64::min) } else { None }
    }
}

/// Quality lanes: each is encoded once and fanned out to every device on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    /// Uncompressed PCM (24- or 16-bit): lowest latency, ~2-5 Mbit/s per device.
    Lossless,
    /// Opus with heavier FEC: ~1 Mbit/s per device, for lossy links or crowds.
    Compact,
}

/// Lossless for up to this many devices; beyond it new devices start on Opus so a shared Wi-Fi
/// network doesn't drown (each device still moves up/down on its own loss).
const LOSSLESS_CROWD: usize = 5;

/// One device we stream to.
struct Peer {
    name: String,
    /// The receiver's addresses we've heard of (one per network link it's reachable on).
    paths: HashMap<SocketAddr, Path>,
    current: Option<SocketAddr>,
    /// --to: we send even if pings are blocked.
    fixed: bool,
    lane: Lane,
    last_report: Option<Report>,
    loss_pct: f64,
    bad: u32,
    good: u32,
    last_heard: Instant,
    /// Latency without any sync delay, ms (for sync mode).
    base_latency_ms: f64,
    /// Extra delay we ask this device to add in sync mode, µs.
    sync_us: u32,
}

impl Peer {
    fn new(name: String, lane: Lane) -> Self {
        Peer {
            name,
            paths: HashMap::new(),
            current: None,
            fixed: false,
            lane,
            last_report: None,
            loss_pct: 0.0,
            bad: 0,
            good: 0,
            last_heard: Instant::now(),
            base_latency_ms: 0.0,
            sync_us: 0,
        }
    }

    fn rtt(&self) -> f64 {
        self.current.and_then(|c| self.paths.get(&c)).and_then(Path::score).unwrap_or(0.0)
    }

    /// Pick the lowest-RTT path; only hop for a clear win so two similar links don't flap.
    fn reselect(&mut self, sh: &Shared) {
        let best = self.paths.iter().filter_map(|(a, p)| p.score().map(|s| (*a, s))).min_by(|a, b| a.1.total_cmp(&b.1));
        let cur_score = self.current.and_then(|c| self.paths.get(&c)).and_then(Path::score);
        let switch = match (best, cur_score) {
            (Some((b, _)), _) if self.current.is_none() => Some(b),
            (Some((b, _)), None) if Some(b) != self.current && !self.fixed => Some(b),
            (Some((b, s)), Some(c)) if s + 0.3 < c && s < c * 0.7 => Some(b),
            _ => None,
        };
        if let Some(b) = switch {
            let rtt = self.paths[&b].score().unwrap_or(0.0);
            sh.event(format!("{}: connected via {} ({}, round trip {:.2} ms)", self.name, net::route_name(b.ip()), b.ip(), rtt));
            self.current = Some(b);
        }
    }

    /// The best working path on a *different* link than the current one (e.g. Wi-Fi when the
    /// cable is current). Sending on both means a hiccup on one link costs nothing.
    fn backup(&self) -> Option<SocketAddr> {
        let cur = self.current?;
        self.paths
            .iter()
            .filter(|(a, _)| a.ip() != cur.ip())
            .filter_map(|(a, p)| p.score().map(|s| (*a, s)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(a, _)| a)
    }

    fn live(&self) -> bool {
        self.current.is_some() && (self.fixed || self.last_heard.elapsed() < Duration::from_secs(10))
    }
}

/// An encoder, its own stream id/sequence, and the recent frames for FEC.
struct LaneTx {
    enc: Encoder,
    stream_id: u32,
    seq: u32,
    redundancy: usize,
    history: VecDeque<(u32, u8, Vec<u8>)>,
    /// Samples waiting for a full frame (Opus frames are longer than capture frames).
    pending: Vec<f32>,
    pending_flags: u8,
}

impl LaneTx {
    fn new(codec: Codec, rate: u32, ch: usize, kbps: u32, redundancy: usize) -> Result<Self> {
        Ok(LaneTx {
            enc: Encoder::new(codec, rate, ch, kbps)?,
            stream_id: net::random_u64() as u32,
            seq: 0,
            redundancy,
            history: VecDeque::new(),
            pending: Vec::new(),
            pending_flags: 0,
        })
    }
}

/// A frame size that divides the OS capture chunk evenly, so no partial frame is left waiting
/// for the next chunk. Picks the divisor of `chunk` closest to `want`, never above `max`.
fn aligned_frame(chunk: usize, want: usize, max: usize) -> usize {
    if chunk < 16 {
        return want;
    }
    (16..=chunk.min(want * 2).min(max))
        .filter(|d| chunk % d == 0)
        .min_by_key(|d| ((*d as i64 - want as i64).abs(), -(*d as i64)))
        .unwrap_or(want)
}

pub fn run(o: Options, sh: Arc<Shared>) -> Result<()> {
    *sh.exclude.lock().unwrap() = o.exclude.clone();
    sh.sync.store(o.sync, Relaxed);
    crate::boost::process();
    crate::boost::thread();
    // Speed up the Windows audio engine *before* capture opens, so loopback starts in small chunks.
    let _engine = if o.source == Source::Loopback && o.engine_boost {
        crate::boost::low_latency_engine(o.device.clone())
    } else {
        None
    };
    if let Some((_, p)) = &_engine {
        let (from, to) = (crate::boost::EnginePeriod::ms(p.default_frames, p.rate), crate::boost::EnginePeriod::ms(p.active_frames, p.rate));
        sh.event(format!("Windows audio engine sped up: {from:.1} → {to:.1} ms"));
        sh.update(|s| {
            s.engine_ms = to;
            s.engine_default_ms = from;
        });
    }
    let mut cap = open_capture(&o)?;
    let system_volume = if o.source == Source::Loopback && o.follow_system_volume { crate::boost::follow_system_volume(o.device.clone()) } else { None };
    if system_volume.is_some() {
        sh.event("Following the Windows volume");
    }
    let mut last_gain = 1.0f32;
    let my_name = net::hostname();
    let wants_voice = o.voice_focus && o.source == Source::Input;
    let mut voice = if wants_voice { crate::voice::VoiceFocus::new(cap.rate) } else { None };
    if wants_voice && voice.is_none() {
        sh.event(format!("Voice focus needs 48 kHz; this mic runs at {} Hz, so it's off", cap.rate));
    }
    // Voice focus works on one channel: a mic is mono anyway, and it halves the bandwidth.
    let wire_ch = if voice.is_some() { 1 } else { cap.channels.min(2) };
    let rate = cap.rate;
    let opus_ok = rate == 48_000 && cfg!(feature = "opus");

    let lossless_codec = match o.codec {
        CodecChoice::Pcm16 => Codec::Pcm16,
        CodecChoice::Opus if opus_ok => Codec::Opus,
        _ => Codec::Pcm24,
    };
    let mut lossless = LaneTx::new(lossless_codec, rate, wire_ch, o.bitrate_kbps, o.redundancy)?;
    let mut compact: Option<LaneTx> = None;

    let sock = net::udp_socket(0, false, false)?;
    sock.set_nonblocking(true)?;
    let mut peers: HashMap<u64, Peer> = HashMap::new();
    if let Some(t) = &o.to {
        let with_port = if t.contains(':') { t.clone() } else { format!("{t}:{}", o.port) };
        let addr = with_port.to_socket_addrs()?.find(|a| a.is_ipv4()).ok_or_else(|| anyhow!("can't resolve {t}"))?;
        let mut p = Peer::new(addr.ip().to_string(), Lane::Lossless);
        p.fixed = true;
        p.current = Some(addr);
        p.paths.insert(addr, Path::default());
        peers.insert(0, p);
    }
    let dir = if o.to.is_none() {
        Some(net::spawn_listener().context("Can't listen for receivers (enter the Mac's IP address instead)")?)
    } else {
        None
    };
    let want_mic = o.port == protocol::MIC_PORT;
    sh.update(|s| {
        s.phase = Phase::Searching;
        s.device_name = cap.name.clone();
        s.sample_rate = rate;
    });
    sh.event(format!("Capturing {} — {} kHz", cap.name, rate as f32 / 1000.0));

    let t0 = Instant::now();
    let mut frame = vec![0f32; 0];
    let mut wire = vec![0f32; 0];
    let mut pkt = Vec::with_capacity(MAX_PACKET);
    let mut rbuf = [0u8; 256];
    let mut last_capture = Instant::now();
    let started = Instant::now();
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut last_status = Instant::now();
    let mut last_info = Instant::now();
    let mut seen_alerts: VecDeque<u32> = VecDeque::new();

    while !sh.stopped() {
        std::thread::park_timeout(Duration::from_millis(1));

        // --- audio out: encode once per lane, fan out to that lane's devices ---
        let mut fs = lossless.enc.frame_samples(rate, wire_ch, o.frame_ms);
        if lossless.enc.codec() != Codec::Opus {
            let max = lossless.enc.frame_samples(rate, wire_ch, 1000.0); // largest that fits a datagram
            fs = aligned_frame(cap.chunk.load(Relaxed) as usize, fs, max);
        }
        let need_compact = peers.values().any(|p| p.lane == Lane::Compact);
        if need_compact && compact.is_none() && opus_ok {
            compact = Some(LaneTx::new(Codec::Opus, rate, wire_ch, o.bitrate_kbps, o.redundancy.max(3))?);
        }
        while cap.cons.slots() >= fs * cap.channels {
            let chunk = cap.cons.read_chunk(fs * cap.channels).unwrap();
            let (a, b) = chunk.as_slices();
            frame.clear();
            frame.extend_from_slice(a);
            frame.extend_from_slice(b);
            chunk.commit_all();
            if let Some(v) = voice.as_mut() {
                to_mono(&frame, cap.channels, &mut wire);
                v.process(&mut wire);
            } else {
                downmix(&frame, cap.channels, &mut wire);
            }
            // In-app slider x Windows volume, glided across the frame so turning the dial is smooth.
            let sys = system_volume.as_ref().map_or(1.0, |v| v.gain());
            let g = sh.volume.get() * sys;
            if g < 0.9999 || last_gain < 0.9999 {
                let frames = (wire.len() / wire_ch.max(1)).max(1);
                for (i, f) in wire.chunks_exact_mut(wire_ch.max(1)).enumerate() {
                    let k = last_gain + (g - last_gain) * (i + 1) as f32 / frames as f32;
                    f.iter_mut().for_each(|s| *s *= k);
                }
            }
            last_gain = g;
            sh.meter(&wire, wire_ch);

            let mut flags = 0;
            if last_capture.elapsed() > Duration::from_millis(40) {
                flags |= FLAG_DISCONT; // PC went quiet and came back; tell the receiver
            }
            last_capture = Instant::now();

            for (lane, tx) in [(Lane::Lossless, Some(&mut lossless)), (Lane::Compact, compact.as_mut())] {
                let Some(tx) = tx else { continue };
                let targets: Vec<SocketAddr> = peers
                    .values()
                    .filter(|p| p.lane == lane && p.live())
                    .flat_map(|p| p.current.into_iter().chain(p.backup().filter(|_| o.multipath)))
                    .collect();
                if targets.is_empty() && lane == Lane::Compact {
                    continue;
                }
                tx.pending.extend_from_slice(&wire);
                tx.pending_flags |= flags;
                let tfs = if tx.enc.codec() == Codec::Opus { tx.enc.frame_samples(rate, wire_ch, o.frame_ms) } else { fs };
                while tx.pending.len() >= tfs * wire_ch {
                    let samples: Vec<f32> = tx.pending.drain(..tfs * wire_ch).collect();
                    let mut bytes = Vec::new();
                    tx.enc.encode(&samples, &mut bytes)?;
                    tx.history.push_front((tx.seq, std::mem::take(&mut tx.pending_flags), bytes));
                    tx.history.truncate(tx.redundancy + 1);
                    tx.seq = tx.seq.wrapping_add(1);
                    let h = AudioHeader { stream_id: tx.stream_id, codec: tx.enc.codec(), channels: wire_ch as u8, sample_rate: rate };
                    for dst in &targets {
                        send_frames(&sock, *dst, &h, &tx.history, &mut pkt, o.sim_loss);
                    }
                }
            }
        }

        // --- control in ---
        loop {
            match sock.recv_from(&mut rbuf) {
                Ok((n, src)) => {
                    let b = &rbuf[..n];
                    let peer = peers.values_mut().find(|p| p.paths.contains_key(&src));
                    if let Some(t) = protocol::parse_ping(protocol::MAGIC_PONG, b) {
                        let rtt = (t0.elapsed().as_micros() as u64).saturating_sub(t) as f64 / 1000.0;
                        if let Some(p) = peer {
                            let path = p.paths.entry(src).or_default();
                            path.rtts.push_back(rtt);
                            if path.rtts.len() > 8 {
                                path.rtts.pop_front();
                            }
                            path.last_pong = Some(Instant::now());
                            p.last_heard = Instant::now();
                        }
                    } else if let Some((id, text)) = protocol::parse_alert(b) {
                        if !seen_alerts.contains(&id) {
                            seen_alerts.push_back(id);
                            if seen_alerts.len() > 32 {
                                seen_alerts.pop_front();
                            }
                            sh.alert_arrived(text);
                        }
                    } else if let (Some(r), Some(p)) = (protocol::parse_report(b), peer) {
                        let lane_id = if p.lane == Lane::Lossless { lossless.stream_id } else { compact.as_ref().map_or(0, |c| c.stream_id) };
                        if r.stream_id != lane_id {
                            continue;
                        }
                        p.last_heard = Instant::now();
                        if let Some(prev) = p.last_report.filter(|prev| prev.stream_id == r.stream_id) {
                            let got = r.received.wrapping_sub(prev.received) + r.concealed.wrapping_sub(prev.concealed);
                            let lost = r.concealed.wrapping_sub(prev.concealed);
                            let fixed = r.recovered.wrapping_sub(prev.recovered);
                            p.loss_pct = if got > 0 { lost as f64 * 100.0 / got as f64 } else { 0.0 };
                            let raw_pct = if got > 0 { (lost + fixed) as f64 * 100.0 / got as f64 } else { 0.0 };
                            if o.codec == CodecChoice::Auto && got > 0 && opus_ok {
                                adapt(p, raw_pct, &sh);
                            }
                        }
                        p.last_report = Some(r);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if net::is_transient(&e) => continue,
                Err(_) => break,
            }
        }

        // --- devices and paths ---
        if last_ping.elapsed() >= Duration::from_millis(500) {
            last_ping = Instant::now();
            let exclude = sh.exclude.lock().unwrap().clone();
            if let Some(dir) = &dir {
                let dir = dir.lock().unwrap();
                for (id, seen) in dir.iter().filter(|(_, s)| s.mic == want_mic) {
                    let wanted = o.name.as_ref().map_or(true, |w| seen.name.to_lowercase().contains(&w.to_lowercase()))
                        && !exclude.iter().any(|x| x == &seen.name);
                    let fresh: Vec<SocketAddr> = seen.addrs.iter().filter(|(_, t)| t.elapsed() < Duration::from_secs(5)).map(|(a, _)| *a).collect();
                    if !wanted || fresh.is_empty() {
                        continue;
                    }
                    let crowd = peers.len() >= LOSSLESS_CROWD && o.codec == CodecChoice::Auto && opus_ok;
                    let p = peers.entry(*id).or_insert_with(|| Peer::new(seen.name.clone(), if crowd { Lane::Compact } else { Lane::Lossless }));
                    for a in fresh {
                        p.paths.entry(a).or_default();
                    }
                }
                // Forget devices that left, were excluded, or stopped answering.
                peers.retain(|id, p| {
                    let keep = p.fixed || (dir.get(id).is_some_and(|s| !exclude.iter().any(|x| x == &s.name)) && (p.current.is_none() || p.last_heard.elapsed() < Duration::from_secs(15)));
                    if !keep {
                        sh.event(format!("{} left", p.name));
                    }
                    keep
                });
            }
            let ping = protocol::write_ping(protocol::MAGIC_PING, t0.elapsed().as_micros() as u64);
            for p in peers.values_mut() {
                for a in p.paths.keys() {
                    let _ = sock.send_to(&ping, a);
                }
                p.reselect(&sh);
            }
        }

        // Once a second: who we are, each device's share of the latency, and (sync mode) how
        // much each device should wait so they all play together.
        if last_info.elapsed() >= Duration::from_secs(1) {
            last_info = Instant::now();
            let sender_ms = (cap.chunk.load(Relaxed) as f64 / cap.rate as f64 + fs as f64 / rate as f64 + if voice.is_some() { crate::voice::DELAY_SECS } else { 0.0 }) * 1000.0;
            for p in peers.values_mut() {
                if let Some(r) = p.last_report {
                    p.base_latency_ms = sender_ms + p.rtt() / 2.0 + r.base_target_us as f64 / 1000.0 + r.device_us as f64 / 1000.0;
                }
            }
            let slowest = peers.values().filter(|p| p.live() && p.last_report.is_some()).map(|p| p.base_latency_ms).fold(0.0, f64::max);
            for p in peers.values_mut() {
                p.sync_us = if sh.sync.load(Relaxed) && p.last_report.is_some() { ((slowest - p.base_latency_ms).max(0.0) * 1000.0) as u32 } else { 0 };
                let Some(dst) = p.current else { continue };
                let stream_id = if p.lane == Lane::Lossless { lossless.stream_id } else { compact.as_ref().map_or(lossless.stream_id, |c| c.stream_id) };
                let info = Info {
                    stream_id,
                    sender_us: (sender_ms * 1000.0) as u32,
                    rtt_us: (p.rtt() * 1000.0) as u32,
                    link: LinkKind::classify(&net::route_name(dst.ip()), dst.ip()).to_u8(),
                    name: my_name.clone(),
                    sync_delay_us: p.sync_us,
                };
                let _ = sock.send_to(&protocol::write_info(&info), dst);
            }
        }

        if last_status.elapsed() >= Duration::from_millis(250) {
            last_status = Instant::now();
            let codec_of = |l: Lane| if l == Lane::Lossless { lossless.enc.codec() } else { Codec::Opus };
            publish(&sh, &peers, codec_of, rate, &cap, fs, last_capture, voice.is_some(), system_volume.as_ref().map(|v| v.gain()));
            // A device that never delivers a single buffer is almost always a permission problem
            // (macOS gives a denied app silence-by-absence, no error). Say so instead of "idle".
            let never = cap.chunk.load(Relaxed) == 0;
            if o.source == Source::Input && never && started.elapsed() > Duration::from_secs(2) {
                let msg = crate::mic_access::silent_mic_message();
                sh.update(|s| s.phase = Phase::Error(msg));
            }
        }
    }
    Ok(())
}

/// Primary frame first, then redundant copies of the previous ones. Everything that fits goes in
/// one datagram; with huge frames (24-bit/192k) the copies spill into a second datagram.
fn send_frames(sock: &std::net::UdpSocket, dst: SocketAddr, h: &AudioHeader, history: &VecDeque<(u32, u8, Vec<u8>)>, pkt: &mut Vec<u8>, sim_loss: f64) {
    let mut i = 0;
    while i < history.len() {
        let start = i;
        let mut size = protocol::AUDIO_HEADER_LEN;
        while i < history.len() && (i == start || size + protocol::FRAME_HEADER_LEN + history[i].2.len() <= MAX_PACKET) {
            size += protocol::FRAME_HEADER_LEN + history[i].2.len();
            i += 1;
        }
        protocol::write_audio_header(pkt, h, (i - start) as u8);
        for (k, (seq, flags, data)) in history.iter().enumerate().take(i).skip(start) {
            let f = if k == 0 { *flags } else { flags | FLAG_REDUNDANT };
            protocol::write_frame(pkt, *seq, f, data);
        }
        if sim_loss <= 0.0 || (net::random_u64() % 10_000) as f64 >= sim_loss * 100.0 {
            let _ = sock.send_to(pkt, dst);
        }
    }
}

/// Auto mode, per device: stay lossless while its link is clean; move it to the Opus lane (with
/// heavier FEC) if it's losing audio, and back once the raw link is clean for 30 s.
fn adapt(p: &mut Peer, raw_pct: f64, sh: &Shared) {
    p.bad = if p.loss_pct > 0.5 { p.bad + 1 } else { 0 };
    p.good = if raw_pct < 0.3 { p.good + 1 } else { 0 };
    if p.lane == Lane::Lossless && p.bad >= 3 {
        p.lane = Lane::Compact;
        p.last_report = None;
        sh.event(format!("{}: link is dropping {:.1}% of audio — switched it to Opus with extra protection", p.name, p.loss_pct));
    } else if p.lane == Lane::Compact && p.good >= 30 {
        p.lane = Lane::Lossless;
        p.last_report = None;
        p.good = 0;
        sh.event(format!("{}: link is clean again — back to lossless", p.name));
    }
}

fn to_mono(input: &[f32], ch: usize, out: &mut Vec<f32>) {
    out.clear();
    let use_ch = ch.clamp(1, 2);
    out.extend(input.chunks_exact(ch).map(|f| f[..use_ch].iter().sum::<f32>() / use_ch as f32));
}

/// Wire format is mono or stereo. Surround layouts fold centre and surrounds into L/R.
fn downmix(input: &[f32], ch: usize, out: &mut Vec<f32>) {
    out.clear();
    if ch <= 2 {
        out.extend_from_slice(input);
        return;
    }
    const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
    for f in input.chunks_exact(ch) {
        let c = f.get(2).copied().unwrap_or(0.0) * K;
        let (sl, sr) = (f.get(4).copied().unwrap_or(0.0) * K, f.get(5).copied().unwrap_or(0.0) * K);
        let (sl2, sr2) = (f.get(6).copied().unwrap_or(0.0) * K, f.get(7).copied().unwrap_or(0.0) * K);
        out.push((f[0] + c + sl + sl2).clamp(-1.0, 1.0));
        out.push((f[1] + c + sr + sr2).clamp(-1.0, 1.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn publish(sh: &Shared, peers: &HashMap<u64, Peer>, codec_of: impl Fn(Lane) -> Codec, rate: u32, cap: &Capture, fs: usize, last_capture: Instant, voice: bool, system_volume: Option<f32>) {
    let flowing = last_capture.elapsed() < Duration::from_millis(500);
    let sender_ms = (cap.chunk.load(Relaxed) as f64 / cap.rate as f64 + fs as f64 / rate as f64 + if voice { crate::voice::DELAY_SECS } else { 0.0 }) * 1000.0;
    let mut list: Vec<PeerStatus> = peers
        .values()
        .filter(|p| p.current.is_some())
        .map(|p| {
            let cur = p.current.unwrap();
            let route = net::route_name(cur.ip());
            let (buffer, device) = p.last_report.map_or((0.0, 0.0), |r| (r.fill_us as f64 / 1000.0, r.device_us as f64 / 1000.0));
            PeerStatus {
                name: p.name.clone(),
                addr: cur.ip().to_string(),
                link: LinkKind::classify(&route, cur.ip()),
                link_name: route,
                codec: codec_of(p.lane).name().into(),
                lossless: codec_of(p.lane) != Codec::Opus,
                rtt_ms: p.rtt() as f32,
                loss_pct: p.loss_pct as f32,
                latency_ms: p.last_report.map_or(0.0, |r| (sender_ms + p.rtt() / 2.0 + buffer.max(r.target_us as f64 / 1000.0) + device) as f32),
                sync_ms: p.sync_us as f32 / 1000.0,
                playing: flowing && p.last_report.is_some() && p.live(),
            }
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    // The headline numbers follow the fastest device that's playing.
    let primary = peers
        .values()
        .filter(|p| p.current.is_some())
        .min_by(|a, b| {
            let key = |p: &Peer| (p.last_report.is_none(), p.rtt());
            key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)
        });
    sh.update(|s| {
        s.capturing = flowing;
        s.system_volume = system_volume;
        s.peers = list;
        let Some(p) = primary else {
            s.phase = Phase::Searching;
            return;
        };
        let cur = p.current.unwrap();
        let route = net::route_name(cur.ip());
        s.phase = if flowing && p.last_report.is_some() { Phase::Streaming } else { Phase::Idle };
        let n = peers.values().filter(|p| p.current.is_some()).count();
        s.peer_name = if n > 1 { format!("{n} devices") } else { p.name.clone() };
        s.peer_addr = cur.ip().to_string();
        s.link = LinkKind::classify(&route, cur.ip());
        s.link_name = route;
        s.codec = codec_of(p.lane).name().into();
        s.sample_rate = rate;
        s.lossless = codec_of(p.lane) != Codec::Opus;
        s.rtt_ms = p.rtt() as f32;
        s.loss_pct = p.loss_pct as f32;
        if let Some(r) = p.last_report {
            s.buffer_ms = r.fill_us as f32 / 1000.0;
            s.target_ms = r.target_us as f32 / 1000.0;
            s.device_ms = r.device_us as f32 / 1000.0;
            // The buffer already holds any sync wait once it has settled at its new target.
            s.latency_ms = (sender_ms + p.rtt() / 2.0) as f32 + s.buffer_ms.max(s.target_ms) + s.device_ms;
            s.fixed = r.recovered;
            s.lost = r.concealed;
            s.underruns = r.underruns;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::aligned_frame;

    #[test]
    fn frames_divide_capture_chunks() {
        assert_eq!(aligned_frame(480, 48, 229), 48); // 10 ms WASAPI @ 48k: 1 ms frames
        assert_eq!(aligned_frame(128, 48, 229), 64); // 2.67 ms engine: 2 frames, nothing left over
        assert_eq!(aligned_frame(441, 44, 229), 49); // 44.1k
        assert_eq!(aligned_frame(0, 48, 229), 48); // unknown yet
        assert_eq!(aligned_frame(48, 48, 229), 48); // tone source
        assert_eq!(aligned_frame(1920, 192, 229), 192); // 192k: stays within one datagram
        assert!(aligned_frame(512, 192, 229) <= 229);
    }
}
