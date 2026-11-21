//! Desktop app: one window for both ends. Lightweight immediate-mode UI (egui), no web engine.
//! Deliberately quiet: nothing animates while streaming, so it never pulls focus.

use crate::control::{self, Control};
use crate::engine::{Phase, Shared, Status};
use crate::playout::Volume;
use crate::sender::{CodecChoice, Source};
use crate::update::{self, Updater};
use crate::{receiver, sender};
use cpal::traits::{DeviceTrait, HostTrait};
use eframe::egui::{self, pos2, vec2, Align, Color32, CornerRadius, FontId, Layout, RichText, Sense, Stroke, Ui};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ------------------------------------------------------------------ palette

const BG: Color32 = Color32::from_rgb(15, 16, 19);
const CARD: Color32 = Color32::from_rgb(23, 24, 28);
const EDGE: Color32 = Color32::from_rgb(38, 40, 46);
const TEXT: Color32 = Color32::from_rgb(232, 232, 234);
const DIM: Color32 = Color32::from_rgb(139, 141, 150);
const ACCENT: Color32 = Color32::from_rgb(110, 200, 184);
const GOOD: Color32 = Color32::from_rgb(107, 200, 140);
const OK: Color32 = Color32::from_rgb(222, 180, 103);
const BAD: Color32 = Color32::from_rgb(222, 107, 111);

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
    /// Receiver: forget latency spikes after 4 s instead of 20 s.
    pub lowest_latency: bool,
    /// Send each packet over two links at once when two are available.
    pub multipath: bool,
    /// Windows: drive the audio engine at its minimum period (see boost.rs).
    pub engine_boost: bool,
    /// Bumped when defaults change in a way saved settings should pick up. Field-level default
    /// (0) so files written before it existed get migrated.
    #[serde(default)]
    pub tuning: u32,
    /// Mac mic: noise suppression + voice leveling before sending.
    pub voice_focus: bool,
    /// Sound awareness (Mac): listen to the room and flag important sounds.
    pub aware: bool,
    pub aware_categories: Vec<crate::aware::Category>,
    /// 0..1, 0.5 = default thresholds.
    pub aware_sensitivity: f32,
    /// Duck the game and mix the room in for a few seconds after an alert.
    pub aware_hear: bool,
    /// Show the alert on the PC's screen (over the game).
    pub aware_pc: bool,
    /// Also post a macOS notification.
    pub aware_notify: bool,
    /// Volume slider position, 0..1 (see `Volume::slider_to_gain`).
    pub volume: f32,
    pub auto_update: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: if cfg!(windows) { Mode::Send } else { Mode::Receive },
            out_device: None,
            min_latency: 2.0,
            buffer: 32,
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
            lowest_latency: false,
            multipath: true,
            engine_boost: true,
            tuning: 1,
            voice_focus: true,
            aware: false,
            aware_categories: crate::aware::Category::ALL.iter().copied().filter(|c| c.on_by_default()).collect(),
            aware_sensitivity: 0.5,
            aware_hear: true,
            aware_pc: true,
            aware_notify: true,
            volume: 1.0,
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
        settings_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|j| Self::from_json(&j)).unwrap_or_default()
    }

    fn from_json(json: &str) -> Self {
        let mut s: Settings = serde_json::from_str(json).unwrap_or_default();
        // v0.4: the Mac device buffer default went 64 -> 32 frames; move people still on the old default.
        if s.tuning < 1 {
            if s.buffer == 64 {
                s.buffer = 32;
            }
            s.tuning = 1;
        }
        s
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
    fn spawn(name: &str, job: Job, gain: f32, ctx: egui::Context) -> Self {
        let shared = Shared::new();
        shared.volume.set(gain); // before the first sample, so a low setting never blasts
        let waker = ctx.clone();
        shared.set_waker(move || waker.request_repaint());
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
        let vol = s.volume;
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
                        spike_memory_secs: if s.lowest_latency { 4 } else { 20 },
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
                        multipath: s.multipath,
                        engine_boost: s.engine_boost,
                        voice_focus: false,
                    },
                    sh,
                )
            }),
        };
        Engine::spawn("engine", job, Volume::slider_to_gain(vol), ctx)
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
                        multipath: s.multipath,
                        engine_boost: false,
                        voice_focus: s.voice_focus,
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
                            buffer_frames: 32,
                            name: Some(format!("{} (mic)", crate::net::hostname())),
                            null_output: std::env::var_os("LAGLESS_NULL_OUTPUT").is_some(),
                            spike_memory_secs: 4, // voice: snappy beats steady
                        },
                        sh,
                    )
                })
            }
            _ => return Ok(None),
        };
        Ok(Some(Engine::spawn("mic", job, 1.0, ctx)))
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
    devices: Devices,
    updater: Updater,
    show_settings: bool,
    /// Latency shown in the big number; refreshed once a second so it doesn't flicker.
    shown_latency: (f32, f64),
    /// Volume to restore when unmuting via the speaker icon.
    unmute_to: f32,
    toast: Option<(String, f64)>,
    /// Local control API for other apps on this computer (see control.rs).
    control: Option<Arc<Control>>,
    /// Streaming stopped from another app; the window shows a Resume button.
    paused: bool,
    /// Sound awareness listener (Mac, receive mode).
    listener: Option<crate::aware::Listener>,
    /// Latest room alert: (text, shown until). On the Mac it's a banner in the window; on the
    /// PC it's an always-on-top overlay over whatever is fullscreen.
    alert: Option<(String, f64)>,
    fonts_warm: bool,
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
        let wake = cc.egui_ctx.clone();
        let control = Control::start(settings_path().and_then(|p| p.parent().map(|d| d.to_path_buf())), move || wake.request_repaint());
        let listener = start_listener(&settings, &cc.egui_ctx);
        App {
            control,
            paused: false,
            listener,
            alert: None,
            fonts_warm: false,
            draft: settings.clone(),
            updater: Updater::start(settings.auto_update),
            unmute_to: if settings.volume > 0.0 { settings.volume } else { 0.5 },
            settings,
            engine,
            status: Status::default(),
            mic,
            mic_status: Status::default(),
            mic_problem,
            devices: Devices::scan(),
            show_settings: false,
            shown_latency: (0.0, -10.0),
            toast: None,
        }
    }

    fn restart(&mut self, ctx: &egui::Context) {
        self.settings.save();
        self.paused = false;
        self.engine.stop();
        self.engine = Engine::start(&self.settings, ctx.clone());
        self.restart_mic(ctx);
        self.restart_listener(ctx);
    }

    fn restart_listener(&mut self, ctx: &egui::Context) {
        self.settings.save();
        self.listener = None; // Drop stops it
        self.listener = start_listener(&self.settings, ctx);
    }

    /// Route room alerts: Mac (listener) -> banner, notification, PC; PC (network) -> overlay.
    fn pump_alerts(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let mut fresh: Vec<String> = Vec::new();
        if let Some(l) = &self.listener {
            for a in l.take_alerts() {
                if self.settings.aware_pc {
                    self.engine.shared.send_alert(a.text.clone());
                }
                if self.settings.aware_notify {
                    notify(&a.text);
                }
                fresh.push(a.text);
            }
        }
        fresh.extend(self.engine.shared.take_alerts());
        if let Some(text) = fresh.pop() {
            self.alert = Some((text, now + ALERT_SECS));
        }
        if let Some((text, until)) = self.alert.clone() {
            if now >= until {
                self.alert = None;
            } else {
                if self.settings.mode == Mode::Send {
                    alert_overlay(ctx, &text);
                }
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
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
        let _ = self.engine.shared.take_levels();
        if let Some(e) = self.engine.shared.take_events().pop() {
            self.toast = Some((e, now));
        }
        if let Some(m) = &self.mic {
            self.mic_status = m.shared.snapshot();
            let _ = m.shared.take_levels();
            if let Some(e) = m.shared.take_events().pop() {
                self.toast = Some((format!("Mic: {e}"), now));
            }
        }
        if now - self.shown_latency.1 >= 1.0 {
            self.shown_latency = (self.status.latency_ms, now);
        }
    }

    /// Apply a command from another app, the same way the matching control in the window would.
    fn apply_command(&mut self, cmd: control::Command, ctx: &egui::Context) {
        match cmd {
            control::Command::Volume(v) => {
                self.settings.volume = v;
                self.apply_volume(true);
            }
            control::Command::Mute(mute) => {
                let muted = self.settings.volume <= 0.0;
                if mute && !muted {
                    self.unmute_to = self.settings.volume;
                    self.settings.volume = 0.0;
                } else if !mute && muted {
                    self.settings.volume = self.unmute_to;
                }
                self.apply_volume(true);
            }
            control::Command::Mic(on) => {
                match self.settings.mode {
                    Mode::Receive => self.settings.mic_send = on,
                    Mode::Send => self.settings.mic_receive = on,
                }
                self.draft.mic_send = self.settings.mic_send;
                self.draft.mic_receive = self.settings.mic_receive;
                self.restart_mic(ctx);
            }
            control::Command::Pause(pause) => self.set_paused(pause, ctx),
        }
    }

    fn set_paused(&mut self, pause: bool, ctx: &egui::Context) {
        if pause == self.paused {
            return;
        }
        self.paused = pause;
        if pause {
            self.engine.stop();
            self.status = Status::default();
        } else {
            self.engine = Engine::start(&self.settings, ctx.clone());
        }
    }

    fn control_state(&self) -> control::AppState {
        control::AppState {
            mode: match self.settings.mode {
                Mode::Send => "send",
                Mode::Receive => "receive",
            },
            paused: self.paused,
            volume: self.settings.volume,
            mic_enabled: match self.settings.mode {
                Mode::Send => self.settings.mic_receive,
                Mode::Receive => self.settings.mic_send,
            },
            mic_problem: self.mic_problem.clone(),
        }
    }
}

impl eframe::App for App {
    /// Runs before every frame and also while the window is minimized/hidden (when the control
    /// API wakes it), so other apps' commands apply even if nobody is looking at the window.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_alerts(ctx);
        let Some(control) = self.control.clone() else { return };
        for cmd in control.take_commands() {
            self.apply_command(cmd, ctx);
        }
        control.publish(self.control_state(), &self.engine.shared, self.mic.as_ref().map(|m| &m.shared));
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.fonts_warm {
            warm_fonts(&ctx);
            self.fonts_warm = true;
        }
        let now = ctx.input(|i| i.time);
        self.tick(now);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::symmetric(20, 16)))
            .show(ui, |ui| {
                self.header(ui, &ctx);
                if self.show_settings {
                    ui.add_space(10.0);
                    egui::ScrollArea::vertical().max_height(ui.available_height() - 36.0).show(ui, |ui| self.settings_card(ui, &ctx));
                } else {
                    if let Some((text, _)) = &self.alert {
                        alert_banner(ui, text);
                    }
                    self.status_block(ui);
                    if self.paused {
                        ui.vertical_centered(|ui| {
                            if accent_button(ui, "Resume streaming").clicked() {
                                self.set_paused(false, &ctx);
                            }
                        });
                        ui.add_space(10.0);
                    }
                    self.volume_row(ui);
                    ui.add_space(14.0);
                    self.mic_row(ui, &ctx);
                    if self.settings.mode == Mode::Receive {
                        ui.add_space(8.0);
                        self.aware_row(ui, &ctx);
                    }
                    ui.add_space(10.0);
                    egui::ScrollArea::vertical().max_height(ui.available_height() - 36.0).show(ui, |ui| self.details(ui));
                }
                self.footer(ui, now, &ctx);
            });

        // Nothing animates, so a few frames a second is plenty for live numbers.
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(c) = &self.control {
            c.shutdown();
        }
        self.listener = None;
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
            ui.label(RichText::new("lagless").size(16.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if settings_icon(ui, self.show_settings).on_hover_text("Settings").clicked() {
                    self.show_settings = !self.show_settings;
                    self.draft = self.settings.clone();
                    if self.show_settings {
                        self.devices = Devices::scan();
                    }
                }
                ui.add_space(4.0);
                if let Some(m) = segmented(ui, self.settings.mode) {
                    self.settings.mode = m;
                    self.draft.mode = m;
                    self.restart(ctx);
                }
            });
        });
    }

    /// Big latency number and one line of status. Static: no glow, no motion.
    fn status_block(&self, ui: &mut Ui) {
        let st = &self.status;
        let sending = self.settings.mode == Mode::Send;
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 150.0), Sense::hover());
        let p = ui.painter_at(rect);
        let c = rect.center() - vec2(0.0, 12.0);

        let lat = self.shown_latency.0;
        let (big, unit) = match st.phase {
            Phase::Streaming if lat > 0.0 => (format!("{lat:.0}"), "ms"),
            _ => ("–".to_string(), ""),
        };
        let g = p.layout_no_wrap(big, FontId::proportional(60.0), TEXT);
        let u = p.layout_no_wrap(unit.into(), FontId::proportional(16.0), DIM);
        let w = g.size().x + if unit.is_empty() { 0.0 } else { u.size().x + 4.0 };
        let top = pos2(c.x - w / 2.0, c.y - g.size().y / 2.0);
        let gs = g.size();
        p.galley(top, g, TEXT);
        if !unit.is_empty() {
            p.galley(pos2(top.x + gs.x + 4.0, top.y + gs.y - u.size().y - 10.0), u, DIM);
        }

        let peer = if st.peer_name.is_empty() { None } else { Some(st.peer_name.as_str()) };
        let (dot, line) = match (&st.phase, peer) {
            _ if self.paused => (DIM, "Paused from another app".to_string()),
            (Phase::Streaming, Some(n)) => {
                let fmt = match st.codec.as_str() {
                    "pcm24" => format!("24-bit/{} kHz", fmt_khz(st.sample_rate)),
                    "pcm16" => format!("16-bit/{} kHz", fmt_khz(st.sample_rate)),
                    _ => "Opus".into(),
                };
                let col = if lat < 25.0 { GOOD } else if lat < 45.0 { OK } else { BAD };
                (col, format!("{} {n}  ·  {}  ·  {fmt}", if sending { "To" } else { "From" }, st.link.label()))
            }
            (Phase::Idle, Some(n)) => (DIM, format!("Connected to {n} — {}", if sending { "play something" } else { "nothing playing" })),
            (Phase::Error(_), _) => (BAD, "Something went wrong — see below".into()),
            _ => (DIM, if sending { "Looking for your Mac…" } else { "Waiting for your PC…" }.into()),
        };
        let lg = p.layout_no_wrap(line, FontId::proportional(12.5), DIM);
        let lw = lg.size().x + 12.0;
        let ly = rect.max.y - 22.0;
        let lx = c.x - lw / 2.0;
        p.circle_filled(pos2(lx + 3.0, ly), 3.0, dot);
        p.galley(pos2(lx + 12.0, ly - lg.size().y / 2.0), lg, DIM);
    }

    fn volume_row(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let muted = self.settings.volume <= 0.0;
            if speaker_icon(ui, muted).on_hover_text(if muted { "Unmute" } else { "Mute" }).clicked() {
                if muted {
                    self.settings.volume = self.unmute_to;
                } else {
                    self.unmute_to = self.settings.volume;
                    self.settings.volume = 0.0;
                }
                self.apply_volume(true);
            }
            let pct = format!("{:.0}%", self.settings.volume * 100.0);
            ui.spacing_mut().slider_width = ui.available_width() - 44.0;
            let r = ui.add(egui::Slider::new(&mut self.settings.volume, 0.0..=1.0).show_value(false));
            if r.changed() {
                self.apply_volume(r.drag_stopped() || !r.dragged());
            }
            if r.drag_stopped() {
                self.settings.save();
            }
            ui.label(RichText::new(pct).size(12.0).color(DIM).monospace());
        });
    }

    /// Push the slider to the running engine immediately; optionally persist.
    fn apply_volume(&mut self, save: bool) {
        self.engine.shared.volume.set(Volume::slider_to_gain(self.settings.volume));
        self.draft.volume = self.settings.volume;
        if self.settings.volume > 0.0 {
            self.unmute_to = self.settings.volume;
        }
        if save {
            self.settings.save();
        }
    }

    fn mic_row(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mode = self.settings.mode;
        let ms = self.mic_status.clone();
        let mut toggled = false;
        let needs_cable = mode == Mode::Send && self.settings.mic_receive && self.mic_problem.as_deref() == Some("no virtual cable");
        ui.horizontal(|ui| {
            let (on, title) = match mode {
                Mode::Receive => (&mut self.settings.mic_send, "Send mic to PC"),
                Mode::Send => (&mut self.settings.mic_receive, "Use Mac mic"),
            };
            if toggle(ui, on).clicked() {
                toggled = true;
            }
            ui.label(RichText::new(title).size(13.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, col, tip) = match (&ms.phase, self.mic.is_some()) {
                    _ if needs_cable => ("needs VB-Cable".to_string(), OK, ""),
                    (_, false) => match &self.mic_problem {
                        Some(p) => ("problem".to_string(), BAD, p.as_str()),
                        None => (String::new(), DIM, ""),
                    },
                    (Phase::Error(e), _) => ("problem".to_string(), BAD, e.as_str()),
                    (Phase::Streaming, _) if mode == Mode::Send => ("on · CABLE Output".to_string(), DIM, "Choose \"CABLE Output\" as the microphone in Discord, Zoom, OBS…"),
                    (Phase::Streaming, _) => (format!("on · {:.0} ms", ms.latency_ms), DIM, "Use headphones on the Mac (or your call app's echo cancellation) to avoid echo."),
                    // Mic works but nothing comes back from the PC: the problem is on that side.
                    (Phase::Idle, _) if mode == Mode::Receive && ms.capturing => ("PC isn't playing it".to_string(), OK, "The Mac mic is working and sending. On the PC: open lagless, check \"Use Mac mic\" is on and VB-Cable is installed; over a direct cable, use Settings → Allow lagless on all networks."),
                    (Phase::Idle, _) => ("no mic input".to_string(), OK, ""),
                    _ => ("connecting…".to_string(), DIM, ""),
                };
                let l = ui.label(RichText::new(text).size(11.5).color(col));
                if !tip.is_empty() {
                    l.on_hover_text(tip);
                }
            });
        });
        let mic_error = match &ms.phase {
            Phase::Error(e) if self.mic.is_some() => Some(e.clone()),
            _ => None,
        };
        if let Some(e) = mic_error {
            ui.label(RichText::new(e).size(11.5).color(OK));
            if cfg!(target_os = "macos") && ui.link(RichText::new("Open Microphone settings").size(11.5)).clicked() {
                crate::mic_access::open_settings();
            }
        }
        if needs_cable {
            ui.horizontal(|ui| {
                ui.add_space(44.0);
                if ui.link(RichText::new("Get VB-Cable (free)").size(11.5)).clicked() {
                    ctx.open_url(egui::OpenUrl::new_tab(VB_CABLE_URL));
                }
                ui.label(RichText::new("·").color(DIM));
                if ui.link(RichText::new("Retry").size(11.5)).clicked() {
                    toggled = true;
                }
            });
        }
        if toggled {
            self.draft.mic_send = self.settings.mic_send;
            self.draft.mic_receive = self.settings.mic_receive;
            self.restart_mic(ctx);
        }
    }

    fn aware_row(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mut toggled = false;
        ui.horizontal(|ui| {
            if toggle(ui, &mut self.settings.aware).clicked() {
                toggled = true;
            }
            ui.label(RichText::new("Sound alerts").size(13.0).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (text, col) = match (&self.listener, self.settings.aware) {
                    (_, false) => (String::new(), DIM),
                    (Some(l), _) if l.problem().is_some() => ("problem".to_string(), BAD),
                    _ if crate::aware::HEAR.active() => ("hearing the room…".to_string(), ACCENT),
                    _ => ("listening".to_string(), DIM),
                };
                ui.label(RichText::new(text).size(11.5).color(col))
                    .on_hover_text("Listens to the room through this Mac's mic and tells you about doorbells, knocks, alarms and more while your IEMs are in. Everything stays on this Mac.");
            });
        });
        if let Some(p) = self.listener.as_ref().and_then(|l| l.problem()) {
            ui.label(RichText::new(p).size(11.5).color(OK));
            if cfg!(target_os = "macos") && ui.link(RichText::new("Open Microphone settings").size(11.5)).clicked() {
                crate::mic_access::open_settings();
            }
        }
        if toggled {
            self.draft.aware = self.settings.aware;
            self.restart_listener(ctx);
        }
    }

    fn details(&self, ui: &mut Ui) {
        let st = &self.status;
        egui::CollapsingHeader::new(RichText::new("Details").size(12.0).color(DIM)).id_salt("details").default_open(false).show(ui, |ui| {
            egui::Grid::new("details_grid").num_columns(2).spacing(vec2(14.0, 6.0)).show(ui, |ui| {
                let row = |ui: &mut Ui, k: &str, v: String| {
                    ui.label(RichText::new(k).size(12.0).color(DIM));
                    ui.label(RichText::new(v).size(12.0).color(TEXT));
                    ui.end_row();
                };
                let dash = |s: &str| if s.is_empty() { "–".to_string() } else { s.to_string() };
                row(ui, if self.settings.mode == Mode::Send { "Sending to" } else { "From" }, if st.peer_addr.is_empty() { "–".into() } else { format!("{} ({})", st.peer_name, st.peer_addr) });
                row(ui, "Link", if st.link_name.is_empty() { "–".into() } else { format!("{} · {}", st.link.label(), st.link_name) });
                row(ui, if self.settings.mode == Mode::Send { "Capturing" } else { "Playing on" }, dash(&st.device_name));
                if st.sample_rate > 0 && !st.codec.is_empty() {
                    row(ui, "Format", format!("{} · {} kHz{}", st.codec, fmt_khz(st.sample_rate), if st.lossless { " · lossless" } else { "" }));
                }
                if st.rtt_ms > 0.0 {
                    row(ui, "Round trip", format!("{:.2} ms", st.rtt_ms));
                }
                if st.engine_ms > 0.0 {
                    row(ui, "Windows engine", format!("{:.1} ms (sped up from {:.1})", st.engine_ms, st.engine_default_ms));
                }
                if st.target_ms > 0.0 {
                    row(ui, "Buffer", format!("{:.1} ms (target {:.1})", st.buffer_ms, st.target_ms));
                }
                if st.jitter_ms > 0.0 {
                    row(ui, "Arrival jitter", format!("{:.1} ms", st.jitter_ms));
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
                        for b in [16u32, 32, 64, 128, 256] {
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
                    ui.checkbox(&mut d.voice_focus, "Voice focus");
                    hint(ui, "Keeps voices, removes background noise (keyboard, fans, room) and evens out your level. Adds about 20 ms to the mic only.");
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
            if d.mode == Mode::Receive {
                section(ui, "SOUND ALERTS");
                ui.checkbox(&mut d.aware, "Listen for important sounds in the room");
                ui.horizontal_wrapped(|ui| {
                    for cat in crate::aware::Category::ALL {
                        let mut on = d.aware_categories.contains(&cat);
                        if ui.checkbox(&mut on, cat.label()).changed() {
                            d.aware_categories.retain(|c| *c != cat);
                            if on {
                                d.aware_categories.push(cat);
                            }
                        }
                    }
                });
                ui.add(egui::Slider::new(&mut d.aware_sensitivity, 0.0..=1.0).show_value(false).text("sensitivity"));
                ui.checkbox(&mut d.aware_hear, "Let me hear it (duck the game, mix the room in for a few seconds)");
                ui.checkbox(&mut d.aware_pc, "Show it on the PC screen");
                ui.checkbox(&mut d.aware_notify, "Mac notification");
                hint(ui, "Runs Google's YAMNet sound model on this Mac (about 2% of one CPU core). Audio never leaves the Mac; only the alert text goes to the PC.");
            }
            section(ui, "LATENCY TWEAKS");
            if d.mode == Mode::Receive {
                ui.checkbox(&mut d.lowest_latency, "Lowest latency (may glitch on network hiccups)");
                hint(ui, "Off: a hiccup raises the buffer for ~20 s so it can't repeat. On: only ~4 s.");
            }
            ui.checkbox(&mut d.multipath, "Send over two links at once");
            hint(ui, "With a cable and Wi-Fi both connected, every packet goes both ways and the first to arrive wins.");
            if cfg!(windows) {
                ui.checkbox(&mut d.engine_boost, "Speed up the Windows audio engine");
                let st = &self.status;
                if st.engine_ms > 0.0 {
                    hint(ui, &format!("Active: {:.1} ms instead of {:.1} ms per chunk.", st.engine_ms, st.engine_default_ms));
                } else {
                    hint(ui, "Asks Windows for its smallest audio period (often 2.7 ms instead of 10). Not every driver allows it.");
                }
                #[cfg(windows)]
                if ui.button("Allow lagless on all networks…").clicked() {
                    let msg = match crate::net::open_firewall() {
                        Ok(()) => "Firewall updated — lagless works over a direct cable now".to_string(),
                        Err(e) => format!("Firewall not changed: {e:#}"),
                    };
                    self.toast = Some((msg, ctx.input(|i| i.time)));
                }
                hint(ui, "Windows treats a direct cable as a Public network. Fixes that in one admin prompt; needed for the mic link over a cable.");
            }
            #[cfg(target_os = "macos")]
            {
                if ui.button("Stop AirDrop Wi-Fi hopping…").clicked() {
                    let msg = match crate::net::disable_awdl() {
                        Ok(()) => "AirDrop radio off until restart — fewer Wi-Fi spikes".to_string(),
                        Err(e) => format!("Not changed: {e:#}"),
                    };
                    self.toast = Some((msg, ctx.input(|i| i.time)));
                }
                hint(ui, "On Wi-Fi, AirDrop makes the Mac's radio hop channels about once a second, which causes latency spikes. Lasts until restart.");
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
        let h = 28.0;
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
    }
}

// ------------------------------------------------------------------ widgets

fn style(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = Color32::from_rgb(15, 16, 22);
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT.gamma_multiply(0.75);
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.hyperlink_color = ACCENT;
    let r = CornerRadius::same(8);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = r;
    }
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(28, 31, 41);
    v.widgets.inactive.bg_fill = Color32::from_rgb(28, 31, 41);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(38, 42, 56);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, EDGE);
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
    egui::Frame::new().fill(CARD).corner_radius(CornerRadius::same(10)).inner_margin(3).stroke(Stroke::new(1.0, EDGE)).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        // right_to_left parent: add Receive first so Send renders on the left.
        for (m, label) in [(Mode::Receive, "Receive"), (Mode::Send, "Send")] {
            let on = m == mode;
            let text = RichText::new(label).size(12.0).color(if on { TEXT } else { DIM });
            let b = egui::Button::new(text).fill(if on { EDGE } else { Color32::TRANSPARENT }).corner_radius(CornerRadius::same(7)).stroke(Stroke::NONE);
            if ui.add(b).clicked() && !on {
                out = Some(m);
            }
        }
    });
    out
}

const ALERT_SECS: f64 = 6.0;

fn start_listener(s: &Settings, ctx: &egui::Context) -> Option<crate::aware::Listener> {
    if s.mode != Mode::Receive || !s.aware {
        return None;
    }
    let wake = ctx.clone();
    Some(crate::aware::Listener::start(
        crate::aware::ListenOptions {
            device: s.mic_device.clone(),
            categories: s.aware_categories.clone(),
            sensitivity: s.aware_sensitivity,
            hear: s.aware_hear,
        },
        move || wake.request_repaint(),
    ))
}

/// macOS notification for a room alert (in the background, never blocks the UI).
fn notify(text: &str) {
    #[cfg(target_os = "macos")]
    {
        let clean: String = text.chars().filter(|c| c.is_alphanumeric() || " -'".contains(*c)).collect();
        let script = format!("display notification \"{clean}\" with title \"lagless\" subtitle \"Heard in the room\"");
        let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
    }
    let _ = text;
}

/// The banner shown inside the lagless window.
fn alert_banner(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(ACCENT.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, ACCENT.gamma_multiply(0.6)))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                bell_icon(ui, ACCENT);
                ui.label(RichText::new(text).size(15.0).color(TEXT).strong());
                ui.label(RichText::new("in the room").size(12.0).color(DIM));
            });
        });
}

/// Small always-on-top banner at the top of the PC's screen. Click-through and never takes
/// focus, so it shows over a borderless/windowed-fullscreen game without interrupting it.
fn alert_overlay(ctx: &egui::Context, text: &str) {
    let screen = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
    let size = vec2(360.0, 78.0);
    let builder = egui::ViewportBuilder::default()
        .with_title("lagless alert")
        .with_decorations(false)
        .with_always_on_top()
        .with_transparent(true)
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_resizable(false)
        .with_active(false)
        .with_inner_size(size)
        .with_position(pos2((screen.x - size.x) / 2.0, 40.0));
    let text = text.to_string();
    ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("lagless-alert"), builder, |ui, _| {
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(20, 21, 26, 238))
                .stroke(Stroke::new(1.5, ACCENT))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::symmetric(16, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        bell_icon(ui, ACCENT);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&text).size(17.0).color(TEXT).strong());
                            ui.label(RichText::new("heard in the room").size(11.5).color(DIM));
                        });
                    });
                });
        });
    });
}

/// Rasterise every glyph the UI uses at every size up front. The alert overlay is a second
/// window sharing the font texture; if new glyphs were added to it while the overlay is up
/// (e.g. the banner's text appearing in the main window), the overlay could draw with a stale copy.
fn warm_fonts(ctx: &egui::Context) {
    const CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 .,:;!?'\"()[]/-–—…·%+→×°";
    let p = egui::Painter::new(ctx.clone(), egui::LayerId::background(), egui::Rect::NOTHING);
    for size in [10.5, 11.0, 11.5, 12.0, 12.5, 13.0, 14.0, 15.0, 16.0, 17.0, 60.0] {
        let _ = p.layout_no_wrap(CHARS.to_string(), FontId::proportional(size), TEXT);
        let _ = p.layout_no_wrap(CHARS.to_string(), FontId::monospace(size), TEXT);
    }
}

/// Painted bell (no icon font needed).
fn bell_icon(ui: &mut Ui, col: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    let c = r.center();
    let p = ui.painter();
    let body = vec![
        c + vec2(-7.0, 5.0),
        c + vec2(-5.5, 3.0),
        c + vec2(-5.0, -3.0),
        c + vec2(-2.5, -6.5),
        c + vec2(2.5, -6.5),
        c + vec2(5.0, -3.0),
        c + vec2(5.5, 3.0),
        c + vec2(7.0, 5.0),
    ];
    p.add(egui::Shape::convex_polygon(body, col, Stroke::NONE));
    p.circle_filled(c + vec2(0.0, 7.5), 2.2, col);
    p.circle_filled(c + vec2(0.0, -7.5), 1.3, col);
}

/// Three-slider "settings" glyph, painted (no icon font needed).
fn settings_icon(ui: &mut Ui, on: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::click());
    let col = if on { TEXT } else if resp.hovered() { TEXT } else { DIM };
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
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
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
    let track = if *on { ACCENT } else { EDGE };
    p.rect_filled(r, CornerRadius::same(10), track.gamma_multiply(if *on { 0.9 } else { 1.0 }));
    let x = egui::lerp((r.min.x + 10.0)..=(r.max.x - 10.0), t);
    p.circle_filled(pos2(x, r.center().y), 7.5, if *on { BG } else { DIM });
    resp
}

fn hint(ui: &mut Ui, s: &str) {
    ui.label(RichText::new(s).size(11.0).color(DIM.gamma_multiply(0.8)));
}

fn accent_button(ui: &mut Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(BG)).fill(ACCENT).corner_radius(CornerRadius::same(7)))
}


/// Small painted speaker; slashed when muted.
fn speaker_icon(ui: &mut Ui, muted: bool) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), Sense::click());
    let col = if resp.hovered() { TEXT } else { DIM };
    let p = ui.painter();
    let c = r.center() - vec2(3.0, 0.0);
    let body = vec![c + vec2(-5.0, -2.5), c + vec2(-2.0, -2.5), c + vec2(2.5, -6.0), c + vec2(2.5, 6.0), c + vec2(-2.0, 2.5), c + vec2(-5.0, 2.5)];
    p.add(egui::Shape::convex_polygon(body, col, Stroke::NONE));
    if muted {
        p.line_segment([c + vec2(5.0, -3.5), c + vec2(10.0, 3.5)], Stroke::new(1.5, col));
        p.line_segment([c + vec2(10.0, -3.5), c + vec2(5.0, 3.5)], Stroke::new(1.5, col));
    } else {
        for (rad, a) in [(5.5f32, 0.7f32), (9.0, 0.75)] {
            let pts: Vec<_> = (0..=8).map(|i| {
                let t = -a + 2.0 * a * i as f32 / 8.0;
                c + vec2(2.5 + rad * t.cos(), rad * t.sin())
            }).collect();
            p.add(egui::Shape::line(pts, Stroke::new(1.4, col)));
        }
    }
    resp
}

fn fmt_khz(rate: u32) -> String {
    if rate % 1000 == 0 { format!("{}", rate / 1000) } else { format!("{:.1}", rate as f32 / 1000.0) }
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
            .with_inner_size([400.0, 560.0])
            .with_min_inner_size([360.0, 420.0])
            .with_icon(Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native("lagless", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn migrates_old_buffer_default_only() {
        // Written by v0.3: old default buffer, no `tuning` field.
        let s = Settings::from_json(r#"{"mode":"Receive","buffer":64,"volume":0.4}"#);
        assert_eq!((s.buffer, s.tuning, s.volume), (32, 1, 0.4));
        // A deliberate non-default choice is kept.
        assert_eq!(Settings::from_json(r#"{"buffer":256}"#).buffer, 256);
        // Already migrated: a later choice of 64 sticks.
        assert_eq!(Settings::from_json(r#"{"buffer":64,"tuning":1}"#).buffer, 64);
        // Fresh install.
        assert_eq!(Settings::from_json("").buffer, 32);
    }
}
