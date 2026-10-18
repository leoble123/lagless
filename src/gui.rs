//! Desktop app: one window for both ends. Lightweight immediate-mode UI (egui), no web engine.

use crate::engine::{LinkKind, Phase, Shared, Status};
use crate::sender::{CodecChoice, Source};
use crate::update::{self, Updater};
use crate::{receiver, sender};
use cpal::traits::{DeviceTrait, HostTrait};
use eframe::egui::{
    self, epaint::Mesh, pos2, vec2, Align, Align2, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense, Shape,
    Stroke, StrokeKind, Ui,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ palette

const BG: Color32 = Color32::from_rgb(11, 12, 16);
const CARD: Color32 = Color32::from_rgb(20, 22, 29);
const CARD_EDGE: Color32 = Color32::from_rgb(33, 36, 48);
const TEXT: Color32 = Color32::from_rgb(230, 232, 239);
const DIM: Color32 = Color32::from_rgb(138, 144, 162);
const TEAL: Color32 = Color32::from_rgb(46, 230, 201);
const BLUE: Color32 = Color32::from_rgb(76, 141, 255);
const VIOLET: Color32 = Color32::from_rgb(155, 92, 255);
const PINK: Color32 = Color32::from_rgb(255, 79, 163);
const GOOD: Color32 = Color32::from_rgb(46, 230, 166);
const OK: Color32 = Color32::from_rgb(255, 193, 77);
const BAD: Color32 = Color32::from_rgb(255, 92, 108);

fn latency_color(ms: f32) -> Color32 {
    if ms <= 0.0 {
        BLUE
    } else if ms < 25.0 {
        GOOD
    } else if ms < 45.0 {
        OK
    } else {
        BAD
    }
}

// ------------------------------------------------------------------ settings

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Send,
    Receive,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SourceSel {
    /// What a playback device is playing (None = system default output).
    Loopback(Option<String>),
    Input(Option<String>),
    Tone,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    // receive
    pub out_device: Option<String>,
    pub min_latency: f64,
    pub buffer: u32,
    pub rx_name: String,
    // send
    pub source: SourceSel,
    pub to: String,
    pub quality: CodecChoice,
    pub fec: usize,
    pub frame_ms: f64,
    // mic link (Mac mic -> PC)
    /// Mac side: stream this Mac's mic to the PC.
    pub mic_send: bool,
    pub mic_device: Option<String>,
    /// PC side: play the Mac's mic into a virtual cable.
    pub mic_receive: bool,
    /// None = auto-detect VB-Audio "CABLE Input".
    pub mic_out: Option<String>,
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: if cfg!(windows) { Mode::Send } else { Mode::Receive },
            out_device: None,
            min_latency: 2.0,
            buffer: 64,
            rx_name: String::new(),
            source: if cfg!(windows) { SourceSel::Loopback(None) } else { SourceSel::Input(None) },
            to: String::new(),
            quality: CodecChoice::Auto,
            fec: 1,
            frame_ms: 1.0,
            mic_send: false,
            mic_device: None,
            mic_receive: true,
            mic_out: None,
            auto_update: true,
        }
    }
}

fn settings_path() -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")));
    Some(base?.join("Lagless").join("settings.json"))
}

impl Settings {
    fn load() -> Self {
        settings_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        if let Some(p) = settings_path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_string_pretty(self).unwrap_or_default());
        }
    }
}

// ------------------------------------------------------------------ app

struct Engine {
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
}

type Job = Box<dyn FnOnce(Arc<Shared>) -> anyhow::Result<()> + Send>;

impl Engine {
    fn spawn(name: &str, job: Job, ctx: egui::Context) -> Self {
        let shared = Shared::new();
        let sh = shared.clone();
        let handle = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Err(e) = job(sh.clone()) {
                    sh.update(|st| st.phase = Phase::Error(format!("{e:#}")));
                }
                ctx.request_repaint();
            })
            .ok();
        Engine { shared, handle }
    }

    fn start(s: &Settings, ctx: egui::Context) -> Self {
        let s = s.clone();
        let job: Job = match s.mode {
            Mode::Receive => Box::new(move |sh| {
                receiver::run(
                    receiver::Options {
                        port: crate::protocol::AUDIO_PORT,
                        device: s.out_device.clone(),
                        min_latency_ms: s.min_latency,
                        max_latency_ms: 120.0,
                        buffer_frames: s.buffer,
                        name: Some(s.rx_name.clone()).filter(|n| !n.trim().is_empty()),
                        null_output: std::env::var_os("LAGLESS_NULL_OUTPUT").is_some(),
                    },
                    sh,
                )
            }),
            Mode::Send => Box::new(move |sh| {
                let (source, device) = match &s.source {
                    SourceSel::Loopback(d) => (Source::Loopback, d.clone()),
                    SourceSel::Input(d) => (Source::Input, d.clone()),
                    SourceSel::Tone => (Source::Tone, None),
                };
                sender::run(
                    sender::Options {
                        to: Some(s.to.trim().to_string()).filter(|t| !t.is_empty()),
                        port: crate::protocol::AUDIO_PORT,
                        name: None,
                        codec: s.quality,
                        frame_ms: s.frame_ms,
                        redundancy: s.fec,
                        bitrate_kbps: 320,
                        source,
                        device,
                        sim_loss: 0.0,
                        tone_rate: 48000,
                    },
                    sh,
                )
            }),
        };
        Engine::spawn("engine", job, ctx)
    }

    /// The reverse link: on the Mac (receive mode) send the mic; on the PC (send mode) play it
    /// into a virtual cable that other apps use as their microphone.
    fn start_mic(s: &Settings, ctx: egui::Context) -> Result<Option<Self>, String> {
        let s = s.clone();
        let job: Job = match s.mode {
            Mode::Receive if s.mic_send => Box::new(move |sh| {
                sender::run(
                    sender::Options {
                        to: None,
                        port: crate::protocol::MIC_PORT,
                        name: None,
                        codec: CodecChoice::Auto,
                        frame_ms: 1.0,
                        redundancy: 1,
                        bitrate_kbps: 192,
                        source: if std::env::var_os("LAGLESS_TONE_MIC").is_some() { Source::Tone } else { Source::Input },
                        device: s.mic_device.clone(),
                        sim_loss: 0.0,
                        tone_rate: 48000,
                    },
                    sh,
                )
            }),
            Mode::Send if s.mic_receive => {
                let out = match s.mic_out.clone().or_else(find_virtual_cable) {
                    Some(o) => o,
                    None => return Err("no virtual cable".into()),
                };
                // Playing the mic into the device we capture would loop it straight back.
                if default_output_name().as_deref() == Some(out.as_str()) {
                    return Err(format!("\"{out}\" is this PC's default output — pick a different default speaker in Windows sound settings"));
                }
                Box::new(move |sh| {
                    receiver::run(
                        receiver::Options {
                            port: crate::protocol::MIC_PORT,
                            device: Some(out),
                            min_latency_ms: 2.0,
                            max_latency_ms: 80.0,
                            buffer_frames: 64,
                            name: Some(format!("{} (mic)", crate::net::hostname())),
                            null_output: std::env::var_os("LAGLESS_NULL_OUTPUT").is_some(),
                        },
                        sh,
                    )
                })
            }
            _ => return Ok(None),
        };
        Ok(Some(Engine::spawn("mic", job, ctx)))
    }

    fn stop(&mut self) {
        self.shared.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

/// VB-Audio Virtual Cable's playback side (what apps hear as "CABLE Output").
fn find_virtual_cable() -> Option<String> {
    let host = cpal::default_host();
    let names: Vec<String> = host.output_devices().ok()?.filter_map(|d| d.name().ok()).collect();
    names
        .iter()
        .find(|n| n.to_lowercase().contains("cable input"))
        .or_else(|| names.iter().find(|n| n.to_lowercase().contains("vb-audio")))
        .cloned()
}

fn default_output_name() -> Option<String> {
    cpal::default_host().default_output_device()?.name().ok()
}

const VB_CABLE_URL: &str = "https://vb-audio.com/Cable/";

struct Devices {
    outputs: Vec<String>,
    inputs: Vec<String>,
}

impl Devices {
    fn scan() -> Self {
        let host = cpal::default_host();
        let names = |it: Option<Vec<cpal::Device>>| -> Vec<String> {
            it.unwrap_or_default().iter().filter_map(|d| d.name().ok()).collect()
        };
        Devices {
            outputs: names(host.output_devices().ok().map(|d| d.collect())),
            inputs: names(host.input_devices().ok().map(|d| d.collect())),
        }
    }
}

pub struct App {
    settings: Settings,
    draft: Settings,
    engine: Engine,
    status: Status,
    mic: Option<Engine>,
    mic_status: Status,
    mic_problem: Option<String>,
    mic_level: f32,
    devices: Devices,
    updater: Updater,
    show_settings: bool,
    levels: [f32; 2],
    peaks: [(f32, f64); 2],
    history: VecDeque<f32>,
    last_sample: f64,
    toast: Option<(String, f64)>,
    started: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let settings = Settings::load();
        let engine = Engine::start(&settings, cc.egui_ctx.clone());
        let (mic, mic_problem) = match Engine::start_mic(&settings, cc.egui_ctx.clone()) {
            Ok(m) => (m, None),
            Err(e) => (None, Some(e)),
        };
        App {
            mic,
            mic_status: Status::default(),
            mic_problem,
            mic_level: 0.0,
            draft: settings.clone(),
            updater: Updater::start(settings.auto_update),
            settings,
            engine,
            status: Status::default(),
            devices: Devices::scan(),
            show_settings: false,
            levels: [0.0; 2],
            peaks: [(0.0, 0.0); 2],
            history: VecDeque::new(),
            last_sample: 0.0,
            toast: None,
            started: Instant::now(),
        }
    }

    fn restart(&mut self, ctx: &egui::Context) {
        self.settings.save();
        self.engine.stop();
        self.history.clear();
        self.engine = Engine::start(&self.settings, ctx.clone());
        self.restart_mic(ctx);
    }

    fn restart_mic(&mut self, ctx: &egui::Context) {
        self.settings.save();
        self.mic = None; // Drop stops it
        self.mic_status = Status::default();
        match Engine::start_mic(&self.settings, ctx.clone()) {
            Ok(m) => {
                self.mic = m;
                self.mic_problem = None;
            }
            Err(e) => self.mic_problem = Some(e),
        }
    }

    fn tick(&mut self, now: f64) {
        self.status = self.engine.shared.snapshot();
        if let Some(e) = self.engine.shared.take_events().pop() {
            self.toast = Some((e, now));
        }
        // Meters: instant attack, smooth release; peak markers hold for a second.
        let fresh = self.engine.shared.take_levels();
        for c in 0..2 {
            let db = to_meter(fresh[c]);
            self.levels[c] = if db > self.levels[c] { db } else { (self.levels[c] - 0.025).max(db) };
            if db >= self.peaks[c].0 || now - self.peaks[c].1 > 1.2 {
                self.peaks[c] = (db, now);
            }
        }
        if let Some(m) = &self.mic {
            self.mic_status = m.shared.snapshot();
            let pk = m.shared.take_levels();
            let db = to_meter(pk[0].max(pk[1]));
            self.mic_level = if db > self.mic_level { db } else { (self.mic_level - 0.025).max(db) };
            if let Some(e) = m.shared.take_events().pop() {
                self.toast = Some((format!("Mic: {e}"), now));
            }
        } else {
            self.mic_level = 0.0;
        }
        if now - self.last_sample > 0.25 {
            self.last_sample = now;
            let v = if self.status.phase == Phase::Streaming { self.status.latency_ms } else { 0.0 };
            self.history.push_back(v);
            if self.history.len() > 240 {
                self.history.pop_front();
            }
        }
    }
}

/// Peak (linear) -> 0..1 on a -60..0 dB scale.
fn to_meter(peak: f32) -> f32 {
    if peak <= 1e-6 {
        return 0.0;
    }
    ((20.0 * peak.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = ctx.input(|i| i.time);
        self.tick(now);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(18))
            .show(ui, |ui| {
                self.header(ui, &ctx);
                ui.add_space(6.0);
                self.hero(ui, now);
                ui.add_space(4.0);
                self.meters(ui);
                ui.add_space(12.0);
                self.chips(ui);
                ui.add_space(12.0);
                if !self.show_settings {
                    self.mic_card(ui, &ctx);
                    ui.add_space(10.0);
                }
                if self.show_settings {
                    egui::ScrollArea::vertical().max_height(ui.available_height() - 44.0).show(ui, |ui| self.settings_card(ui, &ctx));
                } else {
                    self.graph(ui);
                    ui.add_space(10.0);
                    self.details(ui);
                }
                self.footer(ui, now, &ctx);
            });

        // Animate smoothly while there's something moving, otherwise idle cheaply.
        let busy = matches!(self.status.phase, Phase::Streaming | Phase::Searching | Phase::Starting)
            || self.levels.iter().any(|l| *l > 0.0)
            || self.mic_level > 0.0;
        ctx.request_repaint_after(if busy { Duration::from_millis(16) } else { Duration::from_millis(250) });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.mic = None;
        self.engine.stop();
        // Quitting with an update downloaded installs it, so the next launch is the new version.
        if matches!(self.updater.state(), update::State::Ready(_)) {
            let _ = self.updater.apply(false);
        }
    }

    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }
}

// ------------------------------------------------------------------ sections

impl App {
    fn header(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
            gradient_circle(ui.painter(), r.center(), 6.0);
            ui.label(RichText::new("lagless").size(21.0).strong().color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if settings_icon(ui, self.show_settings).on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                    self.draft = self.settings.clone();
                    if self.show_settings {
                        self.devices = Devices::scan();
                    }
                }
                ui.add_space(6.0);
                if let Some(m) = segmented(ui, self.settings.mode) {
                    self.settings.mode = m;
                    self.draft.mode = m;
                    self.restart(ctx);
                }
            });
        });
    }

    fn hero(&self, ui: &mut Ui, now: f64) {
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 210.0), Sense::hover());
        let p = ui.painter_at(rect);
        let c = rect.center();
        let st = &self.status;
        let streaming = st.phase == Phase::Streaming;
        let col = match st.phase {
            Phase::Error(_) => BAD,
            Phase::Streaming => latency_color(st.latency_ms),
            _ => BLUE,
        };
        let level = self.levels[0].max(self.levels[1]);
        let breathe = (now as f32 * 1.3).sin() * 0.5 + 0.5;

        // Glow: many faint stacked discs (smooth falloff), swelling with the music, kept inside
        // the hero area so it never gets clipped flat.
        let reach = rect.height() / 2.0 - 74.0;
        let swell = if streaming { 0.55 + level * 0.45 } else { 0.5 + breathe * 0.15 };
        for i in 0..32 {
            let t = i as f32 / 31.0;
            let r = 74.0 + reach * t * swell.max(0.35);
            p.circle_filled(c, r, col.gamma_multiply(0.035 * (1.0 - t).powf(1.6)));
        }
        p.circle_filled(c, 74.0, CARD);

        // Ring: full gradient ring when streaming, a sweeping comet while searching.
        if streaming {
            gradient_ring(&p, c, 74.0, 4.0, 0.0, TAU, now as f32 * 0.4);
        } else if !matches!(st.phase, Phase::Error(_)) {
            let start = now as f32 * 2.6;
            gradient_ring(&p, c, 74.0, 4.0, start, start + TAU * 0.35, start);
            p.circle_stroke(c, 74.0, Stroke::new(1.0, CARD_EDGE));
        } else {
            p.circle_stroke(c, 74.0, Stroke::new(3.0, BAD));
        }

        let (big, small, caption) = match &st.phase {
            Phase::Streaming if st.latency_ms > 0.0 => (format!("{:.0}", st.latency_ms), "ms", "end-to-end".to_string()),
            Phase::Streaming => ("●".into(), "", "streaming".into()),
            Phase::Idle => ("◌".into(), "", if self.settings.mode == Mode::Send { "play something" } else { "waiting for audio" }.into()),
            Phase::Searching | Phase::Starting => ("…".into(), "", if self.settings.mode == Mode::Send { "finding your Mac" } else { "waiting for your PC" }.into()),
            Phase::Error(_) => ("!".into(), "", "problem".into()),
        };
        let big_font = FontId::proportional(if big.len() <= 3 { 54.0 } else { 40.0 });
        let galley = p.layout_no_wrap(big.clone(), big_font, TEXT);
        let small_g = p.layout_no_wrap(small.into(), FontId::proportional(17.0), DIM);
        let total_w = galley.size().x + if small.is_empty() { 0.0 } else { small_g.size().x + 3.0 };
        let top = pos2(c.x - total_w / 2.0, c.y - galley.size().y / 2.0 - 8.0);
        p.galley(top, galley.clone(), TEXT);
        if !small.is_empty() {
            p.galley(pos2(top.x + galley.size().x + 3.0, top.y + galley.size().y - small_g.size().y - 8.0), small_g, DIM);
        }
        p.text(pos2(c.x, c.y + 30.0), Align2::CENTER_CENTER, caption, FontId::proportional(13.0), col.gamma_multiply(0.9));
    }

    fn meters(&self, ui: &mut Ui) {
        for (c, label) in ["L", "R"].iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(*label).size(11.0).color(DIM).monospace());
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 7.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(r, CornerRadius::same(4), CARD);
                let lit = Rect::from_min_size(r.min, vec2(r.width() * self.levels[c], r.height()));
                if lit.width() > 0.5 {
                    // Gradient spans the full track so loud = pink, like a real meter.
                    gradient_rect(p, lit, r.width(), &[TEAL, BLUE, VIOLET, PINK]);
                }
                let px = r.min.x + r.width() * self.peaks[c].0;
                if self.peaks[c].0 > 0.01 {
                    p.line_segment([pos2(px, r.min.y - 1.0), pos2(px, r.max.y + 1.0)], Stroke::new(2.0, TEXT));
                }
            });
        }
    }

    fn chips(&self, ui: &mut Ui) {
        let st = &self.status;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
            let linked = matches!(st.phase, Phase::Streaming | Phase::Idle);
            if linked {
                let link_col = match st.link {
                    LinkKind::Cable | LinkKind::Thunderbolt | LinkKind::Loopback => GOOD,
                    LinkKind::WiFi | LinkKind::Network => BLUE,
                    LinkKind::Bluetooth => OK,
                };
                chip(ui, st.link.label(), link_col);
                if st.sample_rate > 0 {
                    let bits = match st.codec.as_str() {
                        "pcm24" => "24-bit · ",
                        "pcm16" => "16-bit · ",
                        _ => "",
                    };
                    chip(ui, &format!("{bits}{} kHz", fmt_khz(st.sample_rate)), TEXT);
                }
                if st.lossless {
                    chip(ui, "Lossless", TEAL);
                } else {
                    chip(ui, "Opus 320k", OK);
                }
                if st.rtt_ms > 0.0 {
                    chip(ui, &format!("RTT {:.1} ms", st.rtt_ms), DIM);
                }
            } else {
                chip(ui, if self.settings.mode == Mode::Send { "Sender" } else { "Receiver" }, DIM);
                chip(ui, "Wi-Fi · Ethernet · USB · Bluetooth", DIM);
            }
        });
    }

    fn mic_card(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mode = self.settings.mode;
        let ms = self.mic_status.clone();
        let mut toggled = false;
        card(ui, |ui| {
            ui.horizontal(|ui| {
                let (on, title) = match mode {
                    Mode::Receive => (&mut self.settings.mic_send, "Send this Mac's mic to the PC"),
                    Mode::Send => (&mut self.settings.mic_receive, "Use the Mac's mic on this PC"),
                };
                if toggle(ui, on).clicked() {
                    toggled = true;
                }
                ui.label(RichText::new(title).size(13.0).color(TEXT));
            });
            let active = self.mic.is_some();
            let status = match (&ms.phase, active) {
                (_, false) => match (&self.mic_problem, mode) {
                    (Some(p), Mode::Send) if p == "no virtual cable" => None,
                    (Some(p), _) => Some((p.clone(), BAD)),
                    _ => Some(("Off".into(), DIM)),
                },
                (Phase::Error(e), _) => Some((e.clone(), BAD)),
                (Phase::Streaming, _) => {
                    let lat = if ms.latency_ms > 0.0 { format!(" · {:.0} ms", ms.latency_ms) } else { String::new() };
                    let dir = if mode == Mode::Receive { "to" } else { "from" };
                    Some((format!("{dir} {} via {}{lat}", ms.peer_name, ms.link.label()), GOOD))
                }
                (Phase::Idle, _) => Some(("Connected — no mic audio".into(), OK)),
                _ => Some((if mode == Mode::Receive { "Looking for the PC…" } else { "Waiting for the Mac…" }.into(), BLUE)),
            };
            if let Some((text, col)) = status {
                ui.label(RichText::new(text).size(11.5).color(col));
            }
            if active {
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 5.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(3), BG);
                let lit = Rect::from_min_size(r.min, vec2(r.width() * self.mic_level, r.height()));
                if lit.width() > 0.5 {
                    gradient_rect(ui.painter(), lit, r.width(), &[TEAL, BLUE, VIOLET, PINK]);
                }
            }
            match mode {
                Mode::Send if self.mic_problem.as_deref() == Some("no virtual cable") && self.settings.mic_receive => {
                    hint(ui, "Windows apps need a virtual mic for this. Install the free VB-Audio Virtual Cable, then click Retry.");
                    ui.horizontal(|ui| {
                        if accent_button(ui, "Get VB-Cable").clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(VB_CABLE_URL));
                        }
                        if ui.button("Retry").clicked() {
                            toggled = true;
                        }
                    });
                }
                Mode::Send if active => hint(ui, "In Discord, Zoom, OBS… choose \"CABLE Output\" as your microphone."),
                Mode::Receive if active => hint(ui, "Tip: use headphones/IEMs on the Mac, or the PC app's echo cancellation, to avoid echo."),
                _ => {}
            }
        });
        if toggled {
            self.draft.mic_send = self.settings.mic_send;
            self.draft.mic_receive = self.settings.mic_receive;
            self.restart_mic(ctx);
        }
    }

    fn graph(&self, ui: &mut Ui) {
        card(ui, |ui| {
            let vals: Vec<f32> = self.history.iter().copied().collect();
            let live: Vec<f32> = vals.iter().copied().filter(|v| *v > 0.0).collect();
            ui.horizontal(|ui| {
                ui.label(RichText::new("LATENCY · LAST MINUTE").size(10.5).color(DIM));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let (Some(lo), Some(hi)) = (live.iter().copied().reduce(f32::min), live.iter().copied().reduce(f32::max)) {
                        ui.label(RichText::new(format!("{lo:.0}–{hi:.0} ms")).size(11.0).color(TEXT));
                    }
                });
            });
            let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 64.0), Sense::hover());
            let p = ui.painter_at(r);
            for i in 1..4 {
                let y = r.min.y + r.height() * i as f32 / 4.0;
                p.line_segment([pos2(r.min.x, y), pos2(r.max.x, y)], Stroke::new(1.0, CARD_EDGE.gamma_multiply(0.6)));
            }
            if live.is_empty() {
                p.text(r.center(), Align2::CENTER_CENTER, "no audio yet", FontId::proportional(12.0), DIM.gamma_multiply(0.7));
                return;
            }
            let top = live.iter().copied().fold(20.0f32, f32::max) * 1.25;
            let n = 240usize;
            let x_of = |i: usize| r.max.x - (vals.len() - 1 - i) as f32 * r.width() / (n - 1) as f32;
            let y_of = |v: f32| r.max.y - (v / top) * r.height();
            // Filled area under the line, fading downward.
            let mut mesh = Mesh::default();
            let mut line = Vec::new();
            for (i, &v) in vals.iter().enumerate() {
                if v <= 0.0 {
                    if line.len() > 1 {
                        p.add(Shape::line(std::mem::take(&mut line), Stroke::new(2.0, latency_color(v.max(1.0)))));
                    }
                    line.clear();
                    continue;
                }
                let (x, y) = (x_of(i), y_of(v));
                let col = latency_color(v);
                let idx = mesh.vertices.len() as u32;
                mesh.colored_vertex(pos2(x, y), col.gamma_multiply(0.28));
                mesh.colored_vertex(pos2(x, r.max.y), col.gamma_multiply(0.0));
                if i > 0 && vals[i - 1] > 0.0 && idx >= 2 {
                    mesh.add_triangle(idx - 2, idx - 1, idx);
                    mesh.add_triangle(idx - 1, idx + 1, idx);
                }
                line.push(pos2(x, y));
            }
            p.add(Shape::mesh(mesh));
            if line.len() > 1 {
                let last = *live.last().unwrap();
                p.add(Shape::line(line.clone(), Stroke::new(2.0, latency_color(last))));
                p.circle_filled(*line.last().unwrap(), 3.5, latency_color(last));
            }
        });
    }

    fn details(&self, ui: &mut Ui) {
        let st = &self.status;
        card(ui, |ui| {
            egui::Grid::new("details").num_columns(2).spacing(vec2(14.0, 7.0)).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).size(12.0).color(DIM));
                    ui.label(RichText::new(v).size(12.5).color(TEXT));
                    ui.end_row();
                };
                let peer_label = if self.settings.mode == Mode::Send { "Sending to" } else { "Receiving from" };
                if st.peer_name.is_empty() {
                    row(ui, peer_label, "—".into());
                } else {
                    let via = if st.link_name.is_empty() { String::new() } else { format!(" · {}", st.link_name) };
                    row(ui, peer_label, format!("{}  ({}{via})", st.peer_name, st.peer_addr));
                }
                let dev_label = if self.settings.mode == Mode::Send { "Capturing" } else { "Playing on" };
                row(ui, dev_label, if st.device_name.is_empty() { "—".into() } else { st.device_name.clone() });
                if st.target_ms > 0.0 {
                    row(ui, "Buffer", format!("{:.1} ms  (aiming for {:.1})", st.buffer_ms, st.target_ms));
                }
                row(ui, "Dropouts", format!("{} repaired · {} lost · {} rebuffers", st.fixed, st.lost, st.underruns));
            });
        });
    }

    fn settings_card(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        card(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let d = &mut self.draft;
            match d.mode {
                Mode::Receive => {
                    section(ui, "OUTPUT");
                    let label = d.out_device.clone().unwrap_or_else(|| "System default".into());
                    egui::ComboBox::from_id_salt("out").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.out_device, None, "System default");
                        for n in &self.devices.outputs {
                            ui.selectable_value(&mut d.out_device, Some(n.clone()), n);
                        }
                    });
                    section(ui, "LATENCY");
                    ui.add(egui::Slider::new(&mut d.min_latency, 0.0..=40.0).suffix(" ms min buffer").step_by(0.5));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Device buffer").color(DIM));
                        for b in [32u32, 64, 128, 256, 512] {
                            ui.selectable_value(&mut d.buffer, b, b.to_string());
                        }
                    });
                    hint(ui, "Lower = less delay. Raise these if you hear crackles.");
                    section(ui, "MICROPHONE (SENT TO THE PC)");
                    let label = d.mic_device.clone().unwrap_or_else(|| "System default mic".into());
                    egui::ComboBox::from_id_salt("mic").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.mic_device, None, "System default mic");
                        for n in &self.devices.inputs {
                            ui.selectable_value(&mut d.mic_device, Some(n.clone()), n);
                        }
                    });
                    section(ui, "NAME ON THE NETWORK");
                    ui.add(egui::TextEdit::singleline(&mut d.rx_name).hint_text(crate::net::hostname()).desired_width(f32::INFINITY));
                }
                Mode::Send => {
                    section(ui, "CAPTURE");
                    let label = match &d.source {
                        SourceSel::Loopback(None) => "Everything this PC plays".to_string(),
                        SourceSel::Loopback(Some(n)) => format!("Playback: {n}"),
                        SourceSel::Input(None) => "Default input".into(),
                        SourceSel::Input(Some(n)) => format!("Input: {n}"),
                        SourceSel::Tone => "Test tone".into(),
                    };
                    egui::ComboBox::from_id_salt("src").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        if cfg!(windows) {
                            ui.selectable_value(&mut d.source, SourceSel::Loopback(None), "Everything this PC plays");
                            for n in &self.devices.outputs {
                                ui.selectable_value(&mut d.source, SourceSel::Loopback(Some(n.clone())), format!("Playback: {n}"));
                            }
                        }
                        for n in &self.devices.inputs {
                            ui.selectable_value(&mut d.source, SourceSel::Input(Some(n.clone())), format!("Input: {n}"));
                        }
                        ui.selectable_value(&mut d.source, SourceSel::Tone, "Test tone");
                    });
                    section(ui, "QUALITY");
                    ui.horizontal_wrapped(|ui| {
                        ui.selectable_value(&mut d.quality, CodecChoice::Auto, "Auto");
                        ui.selectable_value(&mut d.quality, CodecChoice::Pcm24, "24-bit lossless");
                        ui.selectable_value(&mut d.quality, CodecChoice::Pcm16, "16-bit");
                        ui.selectable_value(&mut d.quality, CodecChoice::Opus, "Opus");
                    });
                    hint(ui, "Auto = 24-bit lossless, falling back to Opus only if the link drops audio.");
                    ui.add(egui::Slider::new(&mut d.fec, 0..=4).text("loss protection"));
                    ui.add(egui::Slider::new(&mut d.frame_ms, 0.5..=5.0).step_by(0.5).suffix(" ms frames"));
                    section(ui, "MAC MIC PLAYS INTO");
                    let label = d.mic_out.clone().unwrap_or_else(|| "Auto (VB-Audio CABLE Input)".into());
                    egui::ComboBox::from_id_salt("micout").width(ui.available_width()).selected_text(label).show_ui(ui, |ui| {
                        ui.selectable_value(&mut d.mic_out, None, "Auto (VB-Audio CABLE Input)");
                        for n in &self.devices.outputs {
                            ui.selectable_value(&mut d.mic_out, Some(n.clone()), n);
                        }
                    });
                    section(ui, "RECEIVER");
                    ui.add(egui::TextEdit::singleline(&mut d.to).hint_text("Automatic — or type the Mac's IP").desired_width(f32::INFINITY));
                }
            }
            section(ui, "UPDATES");
            ui.horizontal(|ui| {
                ui.checkbox(&mut d.auto_update, "Update automatically");
                if ui.small_button("Check now").clicked() {
                    self.updater.check_now();
                }
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if accent_button(ui, "Apply").clicked() {
                    let mode = self.settings.mode;
                    self.settings = self.draft.clone();
                    self.settings.mode = mode;
                    self.show_settings = false;
                    self.restart(ctx);
                }
                if ui.button("Cancel").clicked() {
                    self.show_settings = false;
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut Ui, now: f64, ctx: &egui::Context) {
        let h = 34.0;
        let space = ui.available_height() - h;
        if space > 0.0 {
            ui.add_space(space);
        }
        ui.horizontal(|ui| {
            // Error > fresh event toast > version.
            if let Phase::Error(e) = &self.status.phase {
                ui.label(RichText::new(e).size(12.0).color(BAD));
                if ui.small_button("Retry").clicked() {
                    self.restart(ctx);
                }
                return;
            }
            let fresh = self.toast.as_ref().filter(|(_, t)| now - t < 6.0);
            match fresh {
                Some((msg, t)) => {
                    let a = (1.0 - ((now - t - 4.5).max(0.0) / 1.5)) as f32;
                    ui.label(RichText::new(msg).size(12.0).color(TEXT.gamma_multiply(a)));
                }
                None => {
                    ui.label(RichText::new(format!("v{}", update::VERSION)).size(11.0).color(DIM.gamma_multiply(0.7)));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| match self.updater.state() {
                update::State::Ready(v) => {
                    if accent_button(ui, &format!("Update to {v}")).on_hover_text("Restarts lagless (or just quit — it installs on exit)").clicked() {
                        self.engine.stop();
                        match self.updater.apply(true) {
                            Ok(()) => std::process::exit(0),
                            Err(e) => self.toast = Some((format!("Update failed: {e:#}"), now)),
                        }
                    }
                }
                update::State::Downloading(v) => {
                    ui.label(RichText::new(format!("downloading {v}…")).size(11.0).color(DIM));
                }
                update::State::Checking => {
                    ui.label(RichText::new("checking for updates…").size(11.0).color(DIM));
                }
                _ => {}
            });
        });
        let _ = self.started;
    }
}

// ------------------------------------------------------------------ widgets & drawing

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(15, 16, 22);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = VIOLET.gamma_multiply(0.55);
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.hyperlink_color = TEAL;
    let r = CornerRadius::same(8);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(28, 31, 41);
    v.widgets.inactive.bg_fill = Color32::from_rgb(28, 31, 41);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(38, 42, 56);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BLUE.gamma_multiply(0.6));
    v.widgets.active.weak_bg_fill = Color32::from_rgb(48, 52, 70);
    v.slider_trailing_fill = true;
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 5.0);
    });
}

fn segmented(ui: &mut Ui, mode: Mode) -> Option<Mode> {
    let mut out = None;
    egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(10)).inner_margin(3).stroke(Stroke::new(1.0, CARD_EDGE)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // right_to_left parent: add Receive first so Send renders on the left.
        for (m, label) in [(Mode::Receive, "Receive"), (Mode::Send, "Send")] {
            let on = m == mode;
            let text = RichText::new(label).size(12.5).color(if on { BG } else { DIM }).strong();
            let b = egui::Button::new(text).fill(if on { TEAL } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(8)).stroke(Stroke::NONE);
            if ui.add(b).clicked() && !on {
                out = Some(m);
            }
        }
    });
    out
}

/// Three-slider "settings" glyph, painted (no icon font needed).
fn settings_icon(ui: &mut Ui, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    let col = if on { TEAL } else if resp.hovered() { TEXT } else { DIM };
    let p = ui.painter();
    for (k, knob) in [0.3f32, 0.7, 0.45].iter().enumerate() {
        let y = r.min.y + 7.0 + k as f32 * 6.0;
        let (x0, x1) = (r.min.x + 5.0, r.max.x - 5.0);
        p.line_segment([pos2(x0, y), pos2(x1, y)], Stroke::new(1.6, col));
        p.circle_filled(pos2(x0 + (x1 - x0) * knob, y), 2.6, col);
    }
    resp
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, CARD_EDGE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn chip(ui: &mut Ui, text: &str, col: Color32) {
    egui::Frame::new()
        .fill(col.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, col.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(20))
        .inner_margin(egui::Margin::symmetric(9, 3))
        .show(ui, |ui| ui.label(RichText::new(text).size(11.5).color(col)));
}

fn section(ui: &mut Ui, s: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(s).size(10.5).color(DIM).strong());
}

/// iOS-style switch with an animated knob.
fn toggle(ui: &mut Ui, on: &mut bool) -> egui::Response {
    let (r, mut resp) = ui.allocate_exact_size(vec2(36.0, 20.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let track = if *on { TEAL } else { Color32::from_rgb(44, 48, 62) };
    p.rect_filled(r, CornerRadius::same(10), track.gamma_multiply(if *on { 0.9 } else { 1.0 }));
    let x = egui::lerp((r.min.x + 10.0)..=(r.max.x - 10.0), t);
    p.circle_filled(pos2(x, r.center().y), 7.5, if *on { BG } else { DIM });
    resp
}

fn hint(ui: &mut Ui, s: &str) {
    ui.label(RichText::new(s).size(11.0).color(DIM.gamma_multiply(0.8)));
}

fn accent_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(BG).strong()).fill(TEAL).corner_radius(CornerRadius::same(8)))
}

fn fmt_khz(rate: u32) -> String {
    if rate % 1000 == 0 { format!("{}", rate / 1000) } else { format!("{:.1}", rate as f32 / 1000.0) }
}

fn lerp_color(stops: &[Color32], t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (t as usize).min(stops.len() - 2);
    let f = t - i as f32;
    let (a, b) = (stops[i], stops[i + 1]);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * f) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Horizontal multi-stop gradient; colour position is relative to `full_width` from rect.min.x.
fn gradient_rect(p: &egui::Painter, r: Rect, full_width: f32, stops: &[Color32]) {
    let mut mesh = Mesh::default();
    let steps = 24;
    for s in 0..=steps {
        let x = r.min.x + r.width() * s as f32 / steps as f32;
        let col = lerp_color(stops, (x - r.min.x) / full_width);
        mesh.colored_vertex(pos2(x, r.min.y), col);
        mesh.colored_vertex(pos2(x, r.max.y), col);
        if s > 0 {
            let i = (s * 2) as u32;
            mesh.add_triangle(i - 2, i - 1, i);
            mesh.add_triangle(i - 1, i + 1, i);
        }
    }
    p.add(Shape::mesh(mesh));
}

/// Arc from angle a0 to a1 with the brand gradient rotating by `spin`.
fn gradient_ring(p: &egui::Painter, c: Pos2, r: f32, w: f32, a0: f32, a1: f32, spin: f32) {
    let stops = [TEAL, BLUE, VIOLET, PINK, TEAL];
    let n = ((a1 - a0).abs() / TAU * 120.0).max(8.0) as usize;
    let mut mesh = Mesh::default();
    let partial = (a1 - a0) < TAU - 0.01;
    for s in 0..=n {
        let a = a0 + (a1 - a0) * s as f32 / n as f32;
        let t = ((a - spin) / TAU).rem_euclid(1.0);
        let mut col = lerp_color(&stops, t);
        if partial {
            col = col.gamma_multiply((s as f32 / n as f32).powf(1.5)); // comet tail
        }
        let dir = vec2(a.cos(), a.sin());
        mesh.colored_vertex(c + dir * (r - w / 2.0), col);
        mesh.colored_vertex(c + dir * (r + w / 2.0), col);
        if s > 0 {
            let i = (s * 2) as u32;
            mesh.add_triangle(i - 2, i - 1, i);
            mesh.add_triangle(i - 1, i + 1, i);
        }
    }
    p.add(Shape::mesh(mesh));
}

fn gradient_circle(p: &egui::Painter, c: Pos2, r: f32) {
    let mut mesh = Mesh::default();
    mesh.colored_vertex(c, BLUE);
    let n = 32;
    for s in 0..=n {
        let a = s as f32 / n as f32 * TAU;
        mesh.colored_vertex(c + vec2(a.cos(), a.sin()) * r, lerp_color(&[TEAL, VIOLET, PINK, TEAL], s as f32 / n as f32));
        if s > 0 {
            mesh.add_triangle(0, s as u32, s as u32 + 1);
        }
    }
    p.add(Shape::mesh(mesh));
    let _ = StrokeKind::Inside;
}

fn app_icon() -> egui::IconData {
    let img = image_from_png(include_bytes!("../assets/icon-256.png"));
    img.unwrap_or_default()
}

fn image_from_png(bytes: &[u8]) -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(bytes).ok()
}

pub fn run() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("lagless")
            .with_inner_size([440.0, 800.0])
            .with_min_inner_size([380.0, 680.0])
            .with_icon(Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native("lagless", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
