//! Sender (the PC): capture -> 1 ms frames -> UDP over the fastest available link.

use crate::audio;
use crate::codec::Encoder;
use crate::engine::{LinkKind, Phase, Shared};
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
    let cfg = if loopback { device.default_output_config()? } else { device.default_input_config()? };
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

/// Picks the receiver and, among every address it is reachable on, the one with the lowest RTT.
struct Router {
    fixed: Option<SocketAddr>,
    port: u16,
    want_name: Option<String>,
    dir: Option<net::Directory>,
    receiver: Option<(u64, String)>,
    paths: HashMap<SocketAddr, Path>,
    current: Option<SocketAddr>,
    sh: Arc<Shared>,
}

impl Router {
    fn candidates(&mut self) -> Vec<SocketAddr> {
        if let Some(a) = self.fixed {
            return vec![a];
        }
        let Some(dir) = &self.dir else { return vec![] };
        let dir = dir.lock().unwrap();
        if self.receiver.is_none() {
            let port = self.port;
            let mut found: Vec<_> = dir
                .iter()
                .filter(|(_, s)| s.addrs.keys().any(|a| a.port() == port))
                .filter(|(_, s)| self.want_name.as_ref().map_or(true, |w| s.name.to_lowercase().contains(&w.to_lowercase())))
                .collect();
            found.sort_by_key(|(_, s)| s.first_seen);
            if let Some((id, s)) = found.first() {
                if found.len() > 1 && self.want_name.is_none() {
                    let names: Vec<_> = found.iter().map(|(_, s)| s.name.as_str()).collect();
                    self.sh.event(format!("Several receivers found {names:?}; using \"{}\"", s.name));
                }
                self.receiver = Some((**id, s.name.clone()));
            }
        }
        let Some((id, _)) = &self.receiver else { return vec![] };
        dir.get(id)
            .map(|s| {
                s.addrs
                    .iter()
                    .filter(|(a, t)| a.port() == self.port && t.elapsed() < Duration::from_secs(5))
                    .map(|(a, _)| *a)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn reselect(&mut self) {
        let best = self
            .paths
            .iter()
            .filter_map(|(a, p)| p.score().map(|s| (*a, s)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let cur_score = self.current.and_then(|c| self.paths.get(&c)).and_then(Path::score);
        let switch = match (best, cur_score) {
            (Some((b, _)), _) if self.current.is_none() => Some(b),
            (Some((b, _)), None) if Some(b) != self.current => Some(b),
            // Only hop for a clear win, so two similar links don't flap.
            (Some((b, s)), Some(c)) if s + 0.3 < c && s < c * 0.7 => Some(b),
            _ => None,
        };
        if let Some(b) = switch {
            let rtt = self.paths[&b].score().unwrap_or(0.0);
            self.sh.event(format!("Connected via {} ({}, round trip {:.2} ms)", net::route_name(b.ip()), b.ip(), rtt));
            self.current = Some(b);
        } else if self.current.is_none() && self.fixed.is_some() {
            self.current = self.fixed; // send even if pings are firewalled
        }
    }
}

pub fn run(o: Options, sh: Arc<Shared>) -> Result<()> {
    let mut cap = open_capture(&o)?;
    let my_name = net::hostname();
    let wire_ch = cap.channels.min(2);
    let rate = cap.rate;
    let stream_id = net::random_u64() as u32;

    let start_codec = match o.codec {
        CodecChoice::Auto | CodecChoice::Pcm24 => Codec::Pcm24,
        CodecChoice::Pcm16 => Codec::Pcm16,
        CodecChoice::Opus => Codec::Opus,
    };
    let mut enc = Encoder::new(start_codec, rate, wire_ch, o.bitrate_kbps)?;
    let mut redundancy = o.redundancy;

    let sock = net::udp_socket(0, false, false)?;
    sock.set_nonblocking(true)?;
    let fixed = match &o.to {
        Some(t) => {
            let with_port = if t.contains(':') { t.clone() } else { format!("{t}:{}", o.port) };
            Some(with_port.to_socket_addrs()?.find(|a| a.is_ipv4()).ok_or_else(|| anyhow!("can't resolve {t}"))?)
        }
        None => None,
    };
    let dir = if fixed.is_none() {
        let d = net::spawn_listener().context("Can't listen for receivers (enter the Mac's IP address instead)")?;
        Some(d)
    } else {
        None
    };
    let mut router = Router { fixed, port: o.port, want_name: o.name.clone(), dir, receiver: None, paths: HashMap::new(), current: None, sh: sh.clone() };
    sh.update(|s| {
        s.phase = Phase::Searching;
        s.device_name = cap.name.clone();
        s.sample_rate = rate;
    });
    sh.event(format!("Capturing {} — {} kHz", cap.name, rate as f32 / 1000.0));

    let t0 = Instant::now();
    let mut seq: u32 = 0;
    let mut history: VecDeque<(u32, u8, Vec<u8>)> = VecDeque::new();
    let mut frame = vec![0f32; 0];
    let mut wire = vec![0f32; 0];
    let mut pkt = Vec::with_capacity(MAX_PACKET);
    let mut rbuf = [0u8; 256];
    let mut last_capture = Instant::now();
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut last_status = Instant::now();
    let mut last_report: Option<Report> = None;
    let mut bad_secs = 0u32;
    let mut good_secs = 0u32;
    let mut loss_pct = 0.0;
    let mut last_info = Instant::now();

    while !sh.stopped() {
        std::thread::park_timeout(Duration::from_millis(1));

        // --- audio out ---
        let fs = enc.frame_samples(rate, wire_ch, o.frame_ms);
        while cap.cons.slots() >= fs * cap.channels {
            let chunk = cap.cons.read_chunk(fs * cap.channels).unwrap();
            let (a, b) = chunk.as_slices();
            frame.clear();
            frame.extend_from_slice(a);
            frame.extend_from_slice(b);
            chunk.commit_all();
            downmix(&frame, cap.channels, &mut wire);
            sh.meter(&wire, wire_ch);

            let mut flags = 0;
            if last_capture.elapsed() > Duration::from_millis(40) {
                flags |= FLAG_DISCONT; // PC went quiet and came back; tell the receiver
            }
            last_capture = Instant::now();

            let mut bytes = Vec::new();
            enc.encode(&wire, &mut bytes)?;
            history.push_front((seq, flags, bytes));
            history.truncate(redundancy + 1);
            seq = seq.wrapping_add(1);

            if let Some(dst) = router.current {
                let h = AudioHeader { stream_id, codec: enc.codec(), channels: wire_ch as u8, sample_rate: rate };
                send_frames(&sock, dst, &h, &history, &mut pkt, o.sim_loss);
            }
        }

        // --- control in ---
        loop {
            match sock.recv_from(&mut rbuf) {
                Ok((n, src)) => {
                    let b = &rbuf[..n];
                    if let Some(t) = protocol::parse_ping(protocol::MAGIC_PONG, b) {
                        let rtt = (t0.elapsed().as_micros() as u64).saturating_sub(t) as f64 / 1000.0;
                        let p = router.paths.entry(src).or_default();
                        p.rtts.push_back(rtt);
                        if p.rtts.len() > 8 {
                            p.rtts.pop_front();
                        }
                        p.last_pong = Some(Instant::now());
                    } else if let Some(r) = protocol::parse_report(b) {
                        if r.stream_id != stream_id {
                            continue;
                        }
                        if let Some(prev) = last_report {
                            let got = r.received.wrapping_sub(prev.received) + r.concealed.wrapping_sub(prev.concealed);
                            let lost = r.concealed.wrapping_sub(prev.concealed);
                            let fixed = r.recovered.wrapping_sub(prev.recovered);
                            loss_pct = if got > 0 { lost as f64 * 100.0 / got as f64 } else { 0.0 };
                            let raw_pct = if got > 0 { (lost + fixed) as f64 * 100.0 / got as f64 } else { 0.0 };
                            if o.codec == CodecChoice::Auto && got > 0 {
                                adapt(&mut enc, &mut redundancy, &o, rate, wire_ch, loss_pct, raw_pct, &mut bad_secs, &mut good_secs, &sh)?;
                            }
                        }
                        last_report = Some(r);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if net::is_transient(&e) => continue,
                Err(_) => break,
            }
        }

        // --- path management ---
        if last_ping.elapsed() >= Duration::from_millis(500) {
            last_ping = Instant::now();
            let ping = protocol::write_ping(protocol::MAGIC_PING, t0.elapsed().as_micros() as u64);
            for a in router.candidates() {
                let _ = sock.send_to(&ping, a);
            }
            router.reselect();
        }

        // Tell the receiver who we are and our share of the latency, so both ends can show it.
        if let Some(dst) = router.current.filter(|_| last_info.elapsed() >= Duration::from_secs(1)) {
            last_info = Instant::now();
            let rtt = router.paths.get(&dst).and_then(Path::score).unwrap_or(0.0);
            let info = Info {
                stream_id,
                sender_us: ((cap.chunk.load(Relaxed) as f64 / cap.rate as f64 + fs as f64 / rate as f64) * 1e6) as u32,
                rtt_us: (rtt * 1000.0) as u32,
                link: LinkKind::classify(&net::route_name(dst.ip()), dst.ip()).to_u8(),
                name: my_name.clone(),
            };
            let _ = sock.send_to(&protocol::write_info(&info), dst);
        }

        if last_status.elapsed() >= Duration::from_millis(250) {
            last_status = Instant::now();
            publish(&sh, &router, &enc, rate, &cap, fs, loss_pct, last_report.as_ref(), redundancy, last_capture);
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

/// Auto mode: stay lossless while the link is clean; fall back to Opus + heavier FEC if it isn't.
#[allow(clippy::too_many_arguments)]
fn adapt(enc: &mut Encoder, redundancy: &mut usize, o: &Options, rate: u32, ch: usize, loss_pct: f64, raw_pct: f64, bad: &mut u32, good: &mut u32, sh: &Shared) -> Result<()> {
    // Going down: audible loss (after FEC). Coming back: the raw link itself must be clean,
    // otherwise a marginal link would flip back and forth.
    *bad = if loss_pct > 0.5 { *bad + 1 } else { 0 };
    *good = if raw_pct < 0.3 { *good + 1 } else { 0 };
    if enc.codec() != Codec::Opus && *bad >= 3 && rate == 48000 && cfg!(feature = "opus") {
        *enc = Encoder::new(Codec::Opus, rate, ch, o.bitrate_kbps)?;
        *redundancy = o.redundancy.max(3);
        sh.event(format!("Link is dropping {loss_pct:.1}% of audio — switched to Opus {} kbps with extra protection", o.bitrate_kbps));
    } else if enc.codec() == Codec::Opus && *good >= 30 {
        *enc = Encoder::new(Codec::Pcm24, rate, ch, o.bitrate_kbps)?;
        *redundancy = o.redundancy;
        sh.event("Link is clean again — back to lossless 24-bit");
    }
    Ok(())
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
fn publish(sh: &Shared, router: &Router, enc: &Encoder, rate: u32, cap: &Capture, fs: usize, loss: f64, r: Option<&Report>, redundancy: usize, last_capture: Instant) {
    let Some(cur) = router.current else {
        sh.update(|s| s.phase = Phase::Searching);
        return;
    };
    let rtt = router.paths.get(&cur).and_then(Path::score).unwrap_or(0.0) as f32;
    let capture_ms = cap.chunk.load(Relaxed) as f32 * 1000.0 / cap.rate as f32;
    let frame_ms = fs as f32 * 1000.0 / rate as f32;
    let route = net::route_name(cur.ip());
    let flowing = last_capture.elapsed() < Duration::from_millis(500);
    sh.update(|s| {
        s.phase = if flowing && r.is_some() { Phase::Streaming } else { Phase::Idle };
        s.peer_name = router.receiver.as_ref().map(|r| r.1.clone()).unwrap_or_else(|| cur.ip().to_string());
        s.peer_addr = cur.ip().to_string();
        s.link = LinkKind::classify(&route, cur.ip());
        s.link_name = route.clone();
        s.codec = enc.codec().name().into();
        s.sample_rate = rate;
        s.lossless = enc.codec() != Codec::Opus;
        s.rtt_ms = rtt;
        s.loss_pct = loss as f32;
        s.fec = redundancy;
        if let Some(r) = r {
            s.buffer_ms = r.fill_us as f32 / 1000.0;
            s.target_ms = r.target_us as f32 / 1000.0;
            s.device_ms = r.device_us as f32 / 1000.0;
            s.latency_ms = capture_ms + frame_ms + rtt / 2.0 + s.buffer_ms + s.device_ms;
            s.fixed = r.recovered;
            s.lost = r.concealed;
            s.underruns = r.underruns;
        }
    });
}
