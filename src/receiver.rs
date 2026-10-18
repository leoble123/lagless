//! Receiver (the Mac): network -> reorder/FEC/concealment -> ring buffer -> Playout in the device callback.

use crate::audio;
use crate::codec::Decoder;
use crate::engine::{LinkKind, Phase, Shared};
use crate::net;
use crate::playout::{Playout, Stats};
use crate::protocol::{self, seq_diff, AudioHeader, Codec, Info, Report, FLAG_DISCONT, FLAG_REDUNDANT};
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use rtrb::{Producer, RingBuffer};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct Options {
    pub port: u16,
    pub device: Option<String>,
    pub min_latency_ms: f64,
    pub max_latency_ms: f64,
    pub buffer_frames: u32,
    pub name: Option<String>,
    /// Debug: no sound card, drive playback from a timer instead.
    pub null_output: bool,
}

/// After this many lost frames in a row, stop inventing audio and let the buffer rebuffer.
const MAX_CONCEAL_RUN: u32 = 8;

struct Session {
    header: AudioHeader,
    sender: SocketAddr,
    info: Option<Info>,
    last_packet: Instant,
    /// Worst packet inter-arrival gap this second, and for the last few seconds.
    gap_now_us: u32,
    gap_hist: std::collections::VecDeque<u32>,
    /// Keeps the output alive (a cpal::Stream, or the null output's token).
    _output: Box<dyn std::any::Any>,
    device_name: String,
    prod: Producer<f32>,
    stats: Arc<Stats>,
    shared: Arc<Shared>,
    decoder: Option<Decoder>,
    pending: BTreeMap<u32, (u8, Codec, Vec<u8>)>,
    expected: Option<u32>,
    frame_samples: usize,
    last_frame: Vec<f32>,
    scratch: Vec<f32>,
    conceal_run: u32,
    newest_primary: Option<u32>,
    fec_depth: u32,
    received: u32,
    recovered: u32,
    concealed: u32,
}

pub fn run(o: Options, sh: Arc<Shared>) -> Result<()> {
    let sock = net::udp_socket(o.port, false, false)
        .map_err(|e| anyhow!("Can't listen on UDP port {} ({e}). Is another copy of lagless receiving?", o.port))?;
    sock.set_read_timeout(Some(Duration::from_millis(50)))?;
    let name = o.name.clone().unwrap_or_else(net::hostname);
    let alive = Arc::new(());
    net::spawn_beacon(net::random_u64(), name.clone(), o.port, Arc::downgrade(&alive))?;
    sh.update(|s| {
        s.phase = Phase::Searching;
        s.peer_name.clear();
    });
    sh.event(format!("Receiving as \"{name}\" — start lagless on your PC"));

    let mut session: Option<Session> = None;
    let mut buf = [0u8; 2048];
    let mut last_tick = Instant::now();
    let mut last_report = Instant::now();
    while !sh.stopped() {
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let pkt = &buf[..n];
                if let Some(t) = protocol::parse_ping(protocol::MAGIC_PING, pkt) {
                    let _ = sock.send_to(&protocol::write_ping(protocol::MAGIC_PONG, t), src);
                } else if let Some(info) = protocol::parse_info(pkt) {
                    if let Some(s) = session.as_mut().filter(|s| s.header.stream_id == info.stream_id) {
                        s.info = Some(info);
                    }
                } else if let Some((h, frames)) = protocol::parse_audio(pkt) {
                    // The codec may change mid-stream (auto mode); anything else means a new stream.
                    let key = |h: &AudioHeader| (h.stream_id, h.sample_rate, h.channels);
                    if session.as_ref().map(|s| key(&s.header)) != Some(key(&h)) {
                        session = None; // drop the old output stream before opening a new one
                        match open_session(&o, h, src, &sh) {
                            Ok(s) => session = Some(s),
                            Err(e) => {
                                sh.update(|s| s.phase = Phase::Error(format!("Can't open audio output: {e:#}")));
                                std::thread::sleep(Duration::from_secs(1));
                                continue;
                            }
                        }
                    }
                    let s = session.as_mut().unwrap();
                    s.sender = src;
                    s.note_arrival();
                    s.header.codec = h.codec;
                    s.on_frames(h.codec, &frames);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) if net::is_transient(&e) => {}
            Err(e) => return Err(e.into()),
        }

        if last_tick.elapsed() >= Duration::from_millis(250) {
            last_tick = Instant::now();
            if session.as_ref().is_some_and(|s| s.last_packet.elapsed() > Duration::from_secs(10)) {
                sh.event("Sender went away");
                session = None;
                sh.update(|s| {
                    s.phase = Phase::Searching;
                    s.peer_name.clear();
                });
            }
            if let Some(s) = &mut session {
                let r = s.report();
                if last_report.elapsed() >= Duration::from_secs(1) {
                    last_report = Instant::now();
                    s.roll_gap_window();
                    let _ = sock.send_to(&protocol::write_report(&r), s.sender);
                }
                s.publish(&r);
            }
        }
    }
    Ok(())
}

fn open_session(o: &Options, h: AudioHeader, sender: SocketAddr, sh: &Arc<Shared>) -> Result<Session> {
    if o.null_output {
        return Ok(null_session(o, h, sender, sh));
    }
    let device = audio::find_device(o.device.as_deref(), true)?;
    let ch = h.channels as usize;

    // Prefer a config that runs natively at the sender's rate (cpal will switch the device rate),
    // so the resampler only has to absorb clock drift.
    let mut best: Option<(i32, cpal::SupportedStreamConfig)> = None;
    for r in device.supported_output_configs()? {
        let fmt_score = match r.sample_format() {
            cpal::SampleFormat::F32 => 3,
            cpal::SampleFormat::I32 => 2,
            cpal::SampleFormat::I16 => 1,
            _ => 0,
        };
        let rate_ok = (r.min_sample_rate().0..=r.max_sample_rate().0).contains(&h.sample_rate);
        let ch_score = if r.channels() == 2 { 2 } else if r.channels() as usize >= ch { 1 } else { 0 };
        if !rate_ok || fmt_score == 0 {
            continue;
        }
        let score = fmt_score + ch_score * 10;
        if best.as_ref().map_or(true, |(s, _)| score > *s) {
            best = Some((score, r.with_sample_rate(cpal::SampleRate(h.sample_rate))));
        }
    }
    let cfg = match best {
        Some((_, c)) => c,
        None => device.default_output_config()?,
    };
    let out_rate = cfg.sample_rate().0;
    let out_ch = cfg.channels();
    let fmt = cfg.sample_format();

    let mut attempts = vec![cpal::BufferSize::Default];
    if o.buffer_frames > 0 {
        attempts.insert(0, cpal::BufferSize::Fixed(o.buffer_frames));
    }
    let mut last_err = None;
    for bs in attempts {
        let stats = Arc::new(Stats::default());
        let (prod, cons) = RingBuffer::new(h.sample_rate as usize * ch * 2);
        let mut playout = Playout::new(cons, ch, out_ch as usize, h.sample_rate, out_rate, o.min_latency_ms, o.max_latency_ms, stats.clone(), sh.volume.clone());
        let st = stats.clone();
        let sc = cpal::StreamConfig { channels: out_ch, sample_rate: cpal::SampleRate(out_rate), buffer_size: bs };
        let built = audio::build_output(&device, &sc, fmt, move |out, lat| {
            if let Some(l) = lat {
                st.device_us.store((l * 1e6) as u32, Relaxed);
            }
            playout.process(out);
        });
        match built.and_then(|s| s.play().map(|_| s).map_err(|_| cpal::BuildStreamError::DeviceNotAvailable)) {
            Ok(stream) => {
                let dev = device.name().unwrap_or_default();
                sh.event(format!(
                    "Playing {} kHz {} on {dev}{}",
                    h.sample_rate as f32 / 1000.0,
                    h.codec.name(),
                    if out_rate != h.sample_rate { format!(" (resampled to {} kHz — match rates for bit-perfect)", out_rate / 1000) } else { String::new() }
                ));
                return Ok(Session::new(h, sender, Box::new(stream), dev, prod, stats, sh.clone()));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!("{}", last_err.unwrap()))
}

fn null_session(o: &Options, h: AudioHeader, sender: SocketAddr, sh: &Arc<Shared>) -> Session {
    let ch = h.channels as usize;
    let stats = Arc::new(Stats::default());
    let (prod, cons) = RingBuffer::new(h.sample_rate as usize * ch * 2);
    let mut playout = Playout::new(cons, ch, 2, h.sample_rate, h.sample_rate, o.min_latency_ms, o.max_latency_ms, stats.clone(), sh.volume.clone());
    let alive = Arc::new(());
    let token = Arc::downgrade(&alive);
    let (rate, period) = (h.sample_rate as f64, o.buffer_frames.max(64) as usize);
    stats.device_us.store((period as f64 / rate * 1e6) as u32, Relaxed);
    std::thread::spawn(move || {
        let mut out = vec![0f32; period * 2];
        let start = Instant::now();
        let mut n = 0u64;
        while token.upgrade().is_some() {
            let due = (start.elapsed().as_secs_f64() * rate / period as f64) as u64;
            while n < due {
                playout.process(&mut out);
                n += 1;
            }
            std::thread::sleep(Duration::from_micros(300));
        }
    });
    Session::new(h, sender, Box::new(alive), "null output".into(), prod, stats, sh.clone())
}

impl Session {
    fn new(header: AudioHeader, sender: SocketAddr, output: Box<dyn std::any::Any>, device_name: String, prod: Producer<f32>, stats: Arc<Stats>, shared: Arc<Shared>) -> Self {
        Session {
            header,
            sender,
            info: None,
            device_name,
            shared,
            last_packet: Instant::now(),
            gap_now_us: 0,
            gap_hist: Default::default(),
            _output: output,
            prod,
            stats,
            decoder: None,
            pending: BTreeMap::new(),
            expected: None,
            frame_samples: header.sample_rate as usize / 1000,
            last_frame: Vec::new(),
            scratch: Vec::new(),
            conceal_run: 0,
            newest_primary: None,
            fec_depth: 0,
            received: 0,
            recovered: 0,
            concealed: 0,
        }
    }

    fn note_arrival(&mut self) {
        let gap = self.last_packet.elapsed().as_micros() as u32;
        self.last_packet = Instant::now();
        // Longer gaps are the PC going quiet, not jitter.
        if gap < 200_000 && gap > self.gap_now_us {
            self.gap_now_us = gap;
            if gap > self.stats.arrival_gap_us.load(Relaxed) {
                self.stats.arrival_gap_us.store(gap, Relaxed); // grow immediately
            }
        }
    }

    /// Called once a second: the buffer floor follows the worst gap of the last 5 seconds.
    fn roll_gap_window(&mut self) {
        self.gap_hist.push_back(self.gap_now_us);
        if self.gap_hist.len() > 5 {
            self.gap_hist.pop_front();
        }
        self.gap_now_us = 0;
        let worst = self.gap_hist.iter().copied().max().unwrap_or(0);
        self.stats.arrival_gap_us.store(worst, Relaxed);
    }

    fn on_frames(&mut self, codec: Codec, frames: &[protocol::Frame]) {
        for f in frames.iter().filter(|f| f.flags & FLAG_REDUNDANT == 0) {
            let exp = *self.expected.get_or_insert(f.seq);
            // Way out of range (sender restarted its counter, or a very long outage): resync.
            if seq_diff(f.seq, exp).unsigned_abs() > 2000 {
                self.pending.clear();
                self.expected = Some(f.seq);
                self.newest_primary = None;
            }
            if self.newest_primary.map_or(true, |n| seq_diff(f.seq, n) > 0) {
                self.newest_primary = Some(f.seq);
            }
        }
        let Some(exp) = self.expected else { return };
        for f in frames {
            if f.flags & FLAG_REDUNDANT != 0 {
                if let Some(n) = self.newest_primary {
                    self.fec_depth = self.fec_depth.max(seq_diff(n, f.seq).clamp(0, 16) as u32);
                }
            }
            if seq_diff(f.seq, exp) < 0 || self.pending.contains_key(&f.seq) {
                continue;
            }
            if f.flags & FLAG_REDUNDANT != 0 {
                self.recovered += 1;
            }
            self.pending.insert(f.seq, (f.flags, codec, f.data.to_vec()));
        }
        // A frame is given up on once every packet that could have carried it has gone by
        // (+1 frame of slack for reordering).
        self.drain(self.fec_depth + 2);
    }

    fn drain(&mut self, window: u32) {
        loop {
            let Some(exp) = self.expected else { return };
            if let Some((flags, codec, data)) = self.pending.remove(&exp) {
                self.expected = Some(exp.wrapping_add(1));
                self.conceal_run = 0;
                if flags & FLAG_DISCONT != 0 {
                    self.stats.discont.store(true, Relaxed);
                }
                if self.decode(codec, &data).is_err() {
                    continue;
                }
                self.shared.meter(&self.scratch, self.header.channels as usize);
                self.received += 1;
                self.last_frame.clone_from(&self.scratch);
                self.frame_samples = self.scratch.len() / self.header.channels as usize;
                let frame_us = (self.frame_samples as u64 * 1_000_000 / self.header.sample_rate as u64) as u32;
                self.stats.frame_us.store(frame_us, Relaxed);
                self.push_scratch();
                continue;
            }
            let Some(&newest) = self.pending.keys().next_back() else { return };
            if seq_diff(newest, exp) < window as i32 {
                return;
            }
            // Lost: conceal a short gap, skip a long one.
            self.concealed += 1;
            self.expected = Some(exp.wrapping_add(1));
            if self.conceal_run < MAX_CONCEAL_RUN {
                self.conceal();
            }
            self.conceal_run += 1;
            if let Some(&oldest) = self.pending.keys().next() {
                let gap = seq_diff(oldest, exp.wrapping_add(1));
                if gap > MAX_CONCEAL_RUN as i32 * 2 {
                    self.concealed += gap as u32;
                    self.expected = Some(oldest);
                }
            }
        }
    }

    fn decode(&mut self, codec: Codec, data: &[u8]) -> Result<()> {
        if self.decoder.as_ref().map(|d| d.codec()) != Some(codec) {
            self.decoder = Some(Decoder::new(codec, self.header.channels as usize)?);
        }
        self.decoder.as_mut().unwrap().decode(data, &mut self.scratch)
    }

    fn conceal(&mut self) {
        let fs = self.frame_samples;
        let native = self.decoder.as_mut().is_some_and(|d| d.conceal(fs, &mut self.scratch));
        if !native {
            // PCM: replay the last frame, fading out with each consecutive loss.
            let g = 0.5f32.powi(self.conceal_run as i32 + 1);
            self.scratch.clear();
            self.scratch.extend(self.last_frame.iter().map(|s| s * g));
        }
        self.push_scratch();
    }

    fn push_scratch(&mut self) {
        if self.prod.slots() < self.scratch.len() {
            return; // playback stalled; the device callback will catch up by skipping
        }
        for &s in &self.scratch {
            let _ = self.prod.push(s);
        }
    }

    fn report(&self) -> Report {
        Report {
            stream_id: self.header.stream_id,
            received: self.received,
            recovered: self.recovered,
            concealed: self.concealed,
            underruns: self.stats.underruns.load(Relaxed),
            target_us: self.stats.target_us.load(Relaxed),
            fill_us: self.stats.fill_us.load(Relaxed),
            device_us: self.stats.device_us.load(Relaxed),
        }
    }

    fn publish(&self, r: &Report) {
        let phase = if self.last_packet.elapsed() > Duration::from_millis(500) {
            Phase::Idle
        } else {
            Phase::Streaming
        };
        let ms = |us: u32| us as f32 / 1000.0;
        let info = self.info.clone().unwrap_or_default();
        let link = if self.info.is_some() {
            LinkKind::from_u8(info.link)
        } else {
            LinkKind::classify(&net::route_name(self.sender.ip()), self.sender.ip())
        };
        let total = if self.info.is_some() {
            ms(info.sender_us) + ms(info.rtt_us) / 2.0 + ms(r.fill_us) + ms(r.device_us)
        } else {
            0.0
        };
        let got = (self.received + self.concealed).max(1);
        self.shared.update(|s| {
            s.phase = phase;
            s.peer_name = if info.name.is_empty() { self.sender.ip().to_string() } else { info.name.clone() };
            s.peer_addr = self.sender.ip().to_string();
            s.link = link;
            s.link_name = net::route_name(self.sender.ip());
            s.device_name = self.device_name.clone();
            s.codec = self.header.codec.name().into();
            s.sample_rate = self.header.sample_rate;
            s.lossless = self.header.codec != Codec::Opus;
            s.rtt_ms = ms(info.rtt_us);
            s.loss_pct = self.concealed as f32 * 100.0 / got as f32;
            s.buffer_ms = ms(r.fill_us);
            s.target_ms = ms(r.target_us);
            s.device_ms = ms(r.device_us);
            s.latency_ms = total;
            s.fixed = r.recovered;
            s.lost = r.concealed;
            s.underruns = r.underruns;
            s.fec = self.fec_depth as usize;
        });
    }
}
