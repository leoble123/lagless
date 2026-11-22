//! Desktop app: one window for both ends. Lightweight immediate-mode UI (egui), no web engine.
//! iOS-style look (grouped lists, switches, a pushed settings page). Deliberately quiet: only
//! interactions animate, nothing moves while you listen.

use crate::control::{self, Control};
use crate::engine::{Phase, Shared, Status};
use crate::playout::Volume;
use crate::sender::{CodecChoice, Source};
use crate::update::{self, Updater};
use crate::{receiver, sender};
use cpal::traits::{DeviceTrait, HostTrait};
use eframe::egui::{self, pos2, vec2, Align, Color32, CornerRadius, FontFamily, FontId, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

// ------------------------------------------------------------------ palette

const BG: Color32 = Color32::from_rgb(0, 0, 0);
const GROUP: Color32 = Color32::from_rgb(28, 28, 30);
const RAISED: Color32 = Color32::from_rgb(44, 44, 46);
const SEP: Color32 = Color32::from_rgb(56, 56, 58);
const TRACK: Color32 = Color32::from_rgb(57, 57, 61);
const TEXT: Color32 = Color32::from_rgb(245, 245, 247);
const DIM: Color32 = Color32::from_rgb(142, 142, 147);
const GRAY: Color32 = Color32::from_rgb(99, 99, 102);
const ACCENT: Color32 = Color32::from_rgb(10, 132, 255);
const GOOD: Color32 = Color32::from_rgb(48, 209, 88);
const OK: Color32 = Color32::from_rgb(255, 159, 10);
const BAD: Color32 = Color32::from_rgb(255, 69, 58);
const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const TEAL: Color32 = Color32::from_rgb(48, 176, 199);

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
    /// PC: receivers (by name) not to stream to. Everything else found gets the audio.
    pub excluded_devices: Vec<String>,
    /// PC: make every device play at the same delay (speakers in one room).
    pub sync_devices: bool,
    /// PC: the Windows volume dial/keys/mute also set the stream's level.
    pub follow_windows_volume: bool,
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
            mode: if cfg!(windows) && !cfg!(target_os = "android") { Mode::Send } else { Mode::Receive },
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
            excluded_devices: Vec::new(),
            sync_devices: false,
            follow_windows_volume: true,
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
                        exclude: s.excluded_devices.clone(),
                        sync: s.sync_devices,
                        follow_system_volume: s.follow_windows_volume,
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
                        exclude: Vec::new(),
                        sync: false,
                        follow_system_volume: false,
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
    details_open: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        style(&cc.egui_ctx);
        let mut settings = Settings::load();
        if cfg!(target_os = "android") {
            settings.mode = Mode::Receive;
            settings.aware = false;
        }
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
            details_open: false,
            draft: settings.clone(),
            updater: Updater::start(settings.auto_update),
            unmute_to: if settings.volume > 0.0 { settings.volume } else { 0.5 },
            settings,
            engine,
            status: Status::default(),
            mic,
            mic_status: Status::default(),
            mic_problem,
            // Listed when settings open; at startup it would only slow the first frame (and on
            // Android it goes through Java).
            devices: Devices { outputs: Vec::new(), inputs: Vec::new() },
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

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG)).show(ui, |ui| {
            let full = ui.max_rect();
            // Settings is a page pushed in from the right, iOS style; the main page slides a
            // little the other way and dims underneath it.
            let t = ctx.animate_bool_with_time_and_easing(egui::Id::unique("settings-page"), self.show_settings, 0.32, egui::emath::easing::cubic_out);
            if t < 1.0 {
                let r = full.translate(vec2(-full.width() * 0.28 * t, 0.0));
                let mut page = ui.new_child(egui::UiBuilder::new().max_rect(r).id_salt("main-page"));
                page.set_clip_rect(full);
                self.main_page(&mut page, &ctx);
                if t > 0.0 {
                    ui.painter().rect_filled(full, 0.0, black(t * 0.5));
                }
            }
            if t > 0.0 {
                let r = full.translate(vec2(full.width() * (1.0 - t), 0.0));
                let p = ui.painter().with_clip_rect(full);
                for i in 0..10 {
                    let s = (i + 1) as f32 * 1.6;
                    p.rect_filled(Rect::from_min_max(pos2(r.min.x - s, r.min.y), pos2(r.min.x, r.max.y)), 0.0, black(0.035 * (1.0 - t * 0.6)));
                }
                p.rect_filled(r, 0.0, BG);
                let mut page = ui.new_child(egui::UiBuilder::new().max_rect(r).id_salt("settings-page"));
                page.set_clip_rect(full);
                self.settings_page(&mut page, &ctx);
            }
        });
        self.toast(&ctx, now);

        // Animations and hover repaint on their own (egui asks for frames while anything moves);
        // otherwise a few frames a second is plenty for the live numbers.
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

// ------------------------------------------------------------------ main page

impl App {
    fn main_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().id_salt("main-scroll").auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 10, bottom: 18 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
                self.title_bar(ui);
                ui.add_space(12.0);
                let modes = [(Mode::Send, "Send"), (Mode::Receive, "Receive")];
                if let Some(m) = segmented(ui, "mode", self.settings.mode, &modes, 32.0, SEG_ON_BG) {
                    self.settings.mode = m;
                    self.draft.mode = m;
                    self.restart(ctx);
                }
                ui.add_space(14.0);
                if let Some((text, _)) = &self.alert {
                    alert_banner(ui, text);
                    ui.add_space(12.0);
                }
                self.hero(ui, ctx);
                ui.add_space(12.0);
                self.volume_card(ui);
                ui.add_space(22.0);
                group(ui, "", "", |ui| {
                    self.mic_rows(ui, ctx);
                    if self.settings.mode == Mode::Receive {
                        self.aware_rows(ui, ctx);
                    }
                });
                if self.settings.mode == Mode::Send {
                    self.devices_group(ui);
                }
                self.details_group(ui);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(format!("lagless {}", update::VERSION)).font(reg(11.5)).color(DIM.gamma_multiply(0.6)));
                });
            });
        });
    }

    fn title_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("lagless").font(semi(30.0)).color(TEXT));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if round_button(ui, Glyph::Sliders).on_hover_text("Settings").clicked() {
                    self.show_settings = true;
                    self.draft = self.settings.clone();
                    self.devices = Devices::scan();
                }
                ui.add_space(4.0);
                match self.updater.state() {
                    update::State::Ready(v) => {
                        if pill(ui, "Update", ACCENT, TEXT).on_hover_text(format!("Restart into {v} (or just quit — it installs on exit)")).clicked() {
                            self.engine.stop();
                            match self.updater.apply(true) {
                                Ok(()) => std::process::exit(0),
                                Err(e) => self.toast = Some((format!("Update failed: {e:#}"), ui.input(|i| i.time))),
                            }
                        }
                    }
                    update::State::Available(v) => {
                        if pill(ui, &format!("Get {v}"), ACCENT, TEXT).clicked() {
                            if let Some(url) = self.updater.download_url() {
                                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                            }
                        }
                    }
                    update::State::Downloading(v) => {
                        ui.label(RichText::new(format!("Getting {v}…")).font(reg(12.0)).color(DIM));
                    }
                    _ => {}
                }
            });
        });
    }

    /// The big card: latency, who it's linked to, and how.
    fn hero(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let st = self.status.clone();
        let sending = self.settings.mode == Mode::Send;
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(vec2(w, 188.0), Sense::hover());
        let p = ui.painter().clone();
        p.rect_filled(rect, 20.0, GROUP);

        let lat = self.shown_latency.0;
        let peer = (!st.peer_name.is_empty()).then_some(st.peer_name.as_str());
        let streaming = st.phase == Phase::Streaming && !self.paused;
        let quality = if lat < 25.0 { GOOD } else if lat < 45.0 { OK } else { BAD };
        let (badge, col) = match &st.phase {
            _ if self.paused => ("PAUSED", DIM),
            Phase::Error(_) => ("PROBLEM", BAD),
            Phase::Streaming => ("LIVE", quality),
            Phase::Idle => ("CONNECTED", ACCENT),
            _ => ("SEARCHING", DIM),
        };

        // A soft glow behind the number while live. Static: nothing moves while you listen.
        if streaming {
            glow(&p, rect.center() + vec2(0.0, 2.0), 100.0, col.gamma_multiply(0.2));
        }

        // Badge, top left.
        let bg = one_line(&p, badge, semi(11.0), col, 200.0);
        let bw = bg.size().x + 26.0;
        let br = Rect::from_min_size(rect.min + vec2(16.0, 16.0), vec2(bw, 22.0));
        p.rect_filled(br, 11.0, col.gamma_multiply(0.16));
        p.circle_filled(pos2(br.min.x + 10.0, br.center().y), 3.0, col);
        p.galley(pos2(br.min.x + 17.0, br.center().y - bg.size().y / 2.0), bg, col);

        // Link, top right.
        if peer.is_some() && !self.paused {
            let lg = one_line(&p, st.link.label(), reg(12.5), DIM, 160.0);
            let x = rect.max.x - 16.0 - lg.size().x;
            let y = br.center().y;
            p.galley(pos2(x, y - lg.size().y / 2.0), lg, DIM);
            glyph(&p, pos2(x - 12.0, y), link_glyph(st.link), DIM, 0.75);
        }

        let c = rect.center() + vec2(0.0, 2.0);
        match &st.phase {
            Phase::Streaming if !self.paused && lat > 0.0 => {
                let g = one_line(&p, &format!("{lat:.0}"), light(76.0), TEXT, w);
                let u = one_line(&p, "ms", reg(18.0), DIM, 100.0);
                let total = g.size().x + 6.0 + u.size().x;
                let top = pos2(c.x - total / 2.0, c.y - g.size().y / 2.0 - 8.0);
                let gs = g.size();
                p.galley(top, g, TEXT);
                p.galley(pos2(top.x + gs.x + 6.0, top.y + gs.y - u.size().y - 13.0), u, DIM);
                let fmt = match st.codec.as_str() {
                    "pcm24" => format!("24-bit / {} kHz", fmt_khz(st.sample_rate)),
                    "pcm16" => format!("16-bit / {} kHz", fmt_khz(st.sample_rate)),
                    _ => "Opus".into(),
                };
                let who = peer.unwrap_or(if sending { "your Mac" } else { "your PC" });
                let line = format!("{} {who}  ·  {fmt}", if sending { "To" } else { "From" });
                let lg = one_line(&p, &line, reg(13.5), DIM, w - 32.0);
                p.galley(pos2(c.x - lg.size().x / 2.0, rect.max.y - 34.0), lg, DIM);
            }
            phase => {
                let (head, sub) = match (phase, peer) {
                    _ if self.paused => ("Paused".to_string(), "Stopped from another app".to_string()),
                    (Phase::Error(e), _) => ("Something went wrong".to_string(), e.clone()),
                    (Phase::Idle, Some(n)) | (Phase::Streaming, Some(n)) => (n.to_string(), if sending { "Connected · play something".into() } else { "Connected · nothing playing".into() }),
                    _ if sending => ("Looking for your Mac".to_string(), "Open lagless on the Mac, phone or tablet".to_string()),
                    _ => ("Waiting for your PC".to_string(), "Open lagless on the PC and choose Send".to_string()),
                };
                let hg = one_line(&p, &head, semi(22.0), TEXT, w - 40.0);
                let sg = p.layout(sub, reg(13.0), if matches!(phase, Phase::Error(_)) { BAD } else { DIM }, w - 48.0);
                let has_button = self.paused || matches!(phase, Phase::Error(_));
                let block = hg.size().y + 6.0 + sg.size().y + if has_button { 44.0 } else { 0.0 };
                let mut y = c.y - block / 2.0 + 10.0;
                p.galley(pos2(c.x - hg.size().x / 2.0, y), hg.clone(), TEXT);
                y += hg.size().y + 6.0;
                let sgs = sg.size();
                p.galley(pos2(c.x - sgs.x / 2.0, y), sg, DIM);
                y += sgs.y + 12.0;
                if has_button {
                    let label = if self.paused { "Resume" } else { "Retry" };
                    let tg = one_line(&p, label, semi(14.0), TEXT, 200.0);
                    let r = Rect::from_center_size(pos2(c.x, y + 15.0), vec2(tg.size().x + 36.0, 30.0));
                    let resp = ui.interact(r, ui.scope_id().with("hero-button"), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                    p.rect_filled(r, 15.0, if resp.hovered() { ACCENT.gamma_multiply(1.15) } else { ACCENT });
                    p.galley(r.center() - tg.size() / 2.0, tg, TEXT);
                    if resp.clicked() {
                        if self.paused {
                            self.set_paused(false, ctx);
                        } else {
                            self.restart(ctx);
                        }
                    }
                }
            }
        }
    }

    /// Control Center style volume: one big bar, drag anywhere; tap the speaker to mute.
    fn volume_card(&mut self, ui: &mut Ui) {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 54.0), Sense::click_and_drag());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let icon_zone = rect.min.x + 52.0;
        let mut mute_click = false;
        if let Some(pos) = resp.interact_pointer_pos() {
            if resp.clicked() && pos.x < icon_zone {
                mute_click = true;
            } else if resp.dragged() || resp.clicked() {
                self.settings.volume = ((pos.x - rect.min.x - 8.0) / (rect.width() - 16.0)).clamp(0.0, 1.0);
                self.apply_volume(resp.clicked());
            }
        }
        if mute_click {
            if self.settings.volume <= 0.0 {
                self.settings.volume = self.unmute_to;
            } else {
                self.unmute_to = self.settings.volume;
                self.settings.volume = 0.0;
            }
            self.apply_volume(true);
        }
        if resp.drag_stopped() {
            self.settings.save();
        }
        let v = self.settings.volume;
        let eased = ui.ctx().animate_value_with_time(resp.id, v, 0.14);
        let shown = if resp.dragged() { v } else { eased };

        let p = ui.painter().clone();
        let clip = ui.clip_rect();
        p.rect_filled(rect, 18.0, GROUP);
        let split = rect.min.x + rect.width() * shown;
        let filled = p.with_clip_rect(Rect::from_min_max(rect.min, pos2(split, rect.max.y)).intersect(clip));
        let empty = p.with_clip_rect(Rect::from_min_max(pos2(split, rect.min.y), rect.max).intersect(clip));
        filled.rect_filled(rect, 18.0, Color32::from_rgb(238, 238, 242));
        let muted = v <= 0.0;
        let ic = pos2(rect.min.x + 26.0, rect.center().y);
        let pct = format!("{:.0}%", v * 100.0);
        for (painter, col) in [(&filled, Color32::from_rgb(28, 28, 30)), (&empty, TEXT)] {
            glyph(painter, ic, if muted { Glyph::Muted } else { Glyph::Speaker }, col, 1.0);
            let g = one_line(painter, &pct, semi(14.0), col, 80.0);
            painter.galley(pos2(rect.max.x - 18.0 - g.size().x, rect.center().y - g.size().y / 2.0), g, col);
        }
        let resp = resp.on_hover_text(if muted { "Tap the speaker to unmute" } else { "Drag to set the volume · tap the speaker to mute" });
        let _ = resp;

        // Show the Windows volume alongside when it's being followed and isn't at full.
        let sys = self.status.system_volume.filter(|g| *g < 0.999 && self.settings.mode == Mode::Send);
        if let Some(g) = sys {
            ui.add_space(8.0);
            let txt = if g <= 0.0 { "Windows is muted, so the devices are too".to_string() } else { format!("Also following the Windows volume ({:.0} dB)", 20.0 * g.log10()) };
            caption(ui, &txt);
        }
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

    fn mic_rows(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mode = self.settings.mode;
        let ms = self.mic_status.clone();
        let needs_cable = mode == Mode::Send && self.settings.mic_receive && self.mic_problem.as_deref() == Some("no virtual cable");
        let running = self.mic.is_some();
        let (title, off_text) = match mode {
            Mode::Receive => ("Send mic to PC", "Use this Mac's mic on the PC"),
            Mode::Send => ("Use Mac mic", "The Mac's mic, as a microphone here"),
        };
        let (text, col, tip): (String, Color32, &str) = match (&ms.phase, running) {
            _ if needs_cable => ("Needs VB-Cable, a free virtual mic".into(), OK, ""),
            (_, false) => match &self.mic_problem {
                Some(p) => (p.clone(), BAD, ""),
                None => (off_text.into(), DIM, ""),
            },
            (Phase::Error(e), _) => (e.clone(), BAD, ""),
            (Phase::Streaming, _) if mode == Mode::Send => ("On · pick “CABLE Output” as the mic in Discord, OBS…".into(), GOOD, "Choose \"CABLE Output\" as the microphone in Discord, Zoom, OBS…"),
            (Phase::Streaming, _) => (format!("On · {:.0} ms", ms.latency_ms), GOOD, "Use headphones on the Mac (or your call app's echo cancellation) to avoid echo."),
            // Mic works but nothing comes back from the PC: the problem is on that side.
            (Phase::Idle, _) if mode == Mode::Receive && ms.capturing => ("Sending, but the PC isn't playing it".into(), OK, "The Mac mic is working and sending. On the PC: open lagless, check \"Use Mac mic\" is on and VB-Cable is installed; over a direct cable, use Settings → Allow lagless on all networks."),
            (Phase::Idle, _) => ("No mic input".into(), OK, ""),
            _ => ("Connecting…".into(), DIM, ""),
        };
        let on = match mode {
            Mode::Receive => &mut self.settings.mic_send,
            Mode::Send => &mut self.settings.mic_receive,
        };
        let mut r = row(ui, Some((Glyph::Mic, OK)), title, TEXT, Some((text.as_str(), col)), Trail::Switch(on));
        if !tip.is_empty() {
            r = r.on_hover_text(tip);
        }
        let mut toggled = r.clicked();
        let mic_error = matches!(&ms.phase, Phase::Error(_)) && running;
        if mic_error && cfg!(target_os = "macos") && row(ui, None, "Open Microphone settings", ACCENT, None, Trail::None).clicked() {
            crate::mic_access::open_settings();
        }
        if needs_cable {
            if row(ui, None, "Get VB-Cable (free)", ACCENT, None, Trail::Chevron(String::new())).clicked() {
                ctx.open_url(egui::OpenUrl::new_tab(VB_CABLE_URL));
            }
            if row(ui, None, "Try again", ACCENT, None, Trail::None).clicked() {
                toggled = true;
            }
        }
        if toggled {
            self.draft.mic_send = self.settings.mic_send;
            self.draft.mic_receive = self.settings.mic_receive;
            self.restart_mic(ctx);
        }
    }

    fn aware_rows(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let problem = self.listener.as_ref().and_then(|l| l.problem());
        let (text, col) = match (&problem, self.settings.aware) {
            (_, false) => ("Doorbells, knocks, alarms and more while your IEMs are in".to_string(), DIM),
            (Some(p), _) => (p.to_string(), BAD),
            _ if crate::aware::HEAR.active() => ("Letting the room in…".to_string(), ACCENT),
            _ => ("Listening to the room".to_string(), GOOD),
        };
        let r = row(ui, Some((Glyph::Bell, BAD)), "Sound alerts", TEXT, Some((text.as_str(), col)), Trail::Switch(&mut self.settings.aware))
            .on_hover_text("Listens to the room through this Mac's mic and tells you about doorbells, knocks, alarms and more while your IEMs are in. Everything stays on this Mac.");
        if problem.is_some() && cfg!(target_os = "macos") && row(ui, None, "Open Microphone settings", ACCENT, None, Trail::None).clicked() {
            crate::mic_access::open_settings();
        }
        if r.clicked() {
            self.draft.aware = self.settings.aware;
            self.restart_listener(ctx);
        }
    }

    /// Sender: every device getting the audio. Tap one to stop/start streaming to it.
    fn devices_group(&mut self, ui: &mut Ui) {
        let peers = self.status.peers.clone();
        let mut changed = false;
        let header = if peers.is_empty() { "Devices".to_string() } else { format!("Devices · {}", peers.len()) };
        group(ui, &header, "", |ui| {
            if peers.is_empty() && self.settings.excluded_devices.is_empty() {
                row(ui, Some((Glyph::Wifi, GRAY)), "Looking for devices…", DIM, Some(("Every Mac, phone or tablet running lagless shows up here", DIM)), Trail::None);
            }
            for p in &peers {
                let lat = if p.latency_ms > 0.0 { format!("{:.0} ms", p.latency_ms) } else { "…".into() };
                let q = if p.lossless { "Lossless" } else { "Opus" };
                let (sub, col) = if p.playing { (format!("{} · {q} · {lat}", p.link.label()), DIM) } else { (format!("{} · connecting…", p.link.label()), OK) };
                let tip = format!(
                    "{} via {}\n{} · round trip {:.2} ms · {:.1}% lost{}\nClick to stop streaming to it",
                    p.addr,
                    p.link_name,
                    p.codec,
                    p.rtt_ms,
                    p.loss_pct,
                    if p.sync_ms > 0.5 { format!("\nwaiting +{:.0} ms to stay in sync", p.sync_ms) } else { String::new() }
                );
                if row(ui, Some((device_glyph(&p.name), ACCENT)), &p.name, TEXT, Some((sub.as_str(), col)), Trail::Check(true)).on_hover_text(tip).clicked() {
                    self.settings.excluded_devices.push(p.name.clone());
                    changed = true;
                }
            }
            for name in self.settings.excluded_devices.clone() {
                if row(ui, Some((device_glyph(&name), GRAY)), &name, DIM, Some(("Not streaming · click to add", DIM)), Trail::Check(false)).clicked() {
                    self.settings.excluded_devices.retain(|n| n != &name);
                    changed = true;
                }
            }
            if peers.len() + self.settings.excluded_devices.len() >= 2
                && row(ui, Some((Glyph::Sync, TEAL)), "Play in sync", TEXT, Some(("Speakers in one room won't echo; faster devices wait a little", DIM)), Trail::Switch(&mut self.settings.sync_devices)).clicked()
            {
                changed = true;
            }
        });
        if changed {
            *self.engine.shared.exclude.lock().unwrap() = self.settings.excluded_devices.clone();
            self.engine.shared.sync.store(self.settings.sync_devices, std::sync::atomic::Ordering::Relaxed);
            self.draft.excluded_devices = self.settings.excluded_devices.clone();
            self.draft.sync_devices = self.settings.sync_devices;
            self.settings.save();
        }
    }

    fn details_group(&mut self, ui: &mut Ui) {
        let st = self.status.clone();
        let send = self.settings.mode == Mode::Send;
        group(ui, "", "", |ui| {
            if row(ui, Some((Glyph::Info, GRAY)), "Details", TEXT, None, Trail::Disclose(self.details_open)).clicked() {
                self.details_open = !self.details_open;
            }
            if !self.details_open {
                return;
            }
            let kv = |ui: &mut Ui, k: &str, v: String| {
                row(ui, None, k, DIM, None, Trail::Value(v, TEXT));
            };
            let dash = |s: &str| if s.is_empty() { "–".to_string() } else { s.to_string() };
            kv(ui, if send { "Sending to" } else { "From" }, if st.peer_addr.is_empty() { "–".into() } else { format!("{} ({})", st.peer_name, st.peer_addr) });
            kv(ui, "Link", if st.link_name.is_empty() { "–".into() } else { format!("{} · {}", st.link.label(), st.link_name) });
            kv(ui, if send { "Capturing" } else { "Playing on" }, dash(&st.device_name));
            if st.sample_rate > 0 && !st.codec.is_empty() {
                kv(ui, "Format", format!("{} · {} kHz{}", st.codec, fmt_khz(st.sample_rate), if st.lossless { " · lossless" } else { "" }));
            }
            if st.rtt_ms > 0.0 {
                kv(ui, "Round trip", format!("{:.2} ms", st.rtt_ms));
            }
            if st.engine_ms > 0.0 {
                kv(ui, "Windows engine", format!("{:.1} ms (from {:.1})", st.engine_ms, st.engine_default_ms));
            }
            if st.target_ms > 0.0 {
                kv(ui, "Buffer", format!("{:.1} ms (target {:.1})", st.buffer_ms, st.target_ms));
            }
            if st.jitter_ms > 0.0 {
                kv(ui, "Arrival jitter", format!("{:.1} ms", st.jitter_ms));
            }
            kv(ui, "Repaired", st.fixed.to_string());
            kv(ui, "Lost", st.lost.to_string());
            kv(ui, "Rebuffers", st.underruns.to_string());
        });
    }

    /// Fresh events float up from the bottom as a capsule, then fade.
    fn toast(&mut self, ctx: &egui::Context, now: f64) {
        let Some((msg, t)) = self.toast.clone().filter(|(_, t)| now - t < 6.0) else { return };
        let a = (1.0 - ((now - t - 4.5).max(0.0) / 1.5)) as f32;
        let rise = egui::emath::easing::cubic_out(((now - t) / 0.3).min(1.0) as f32);
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -18.0 + 12.0 * (1.0 - rise)))
            .interactable(false)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(RAISED.gamma_multiply(a))
                    .corner_radius(CornerRadius::same(18))
                    .inner_margin(egui::Margin::symmetric(16, 9))
                    .shadow(egui::Shadow { offset: [0, 4], blur: 18, spread: 0, color: black(0.5 * a) })
                    .show(ui, |ui| {
                        ui.set_max_width(320.0);
                        ui.label(RichText::new(msg).font(reg(13.0)).color(TEXT.gamma_multiply(a)));
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(if now - t < 0.3 || now - t > 4.5 { 16 } else { 250 }));
    }
}

// ------------------------------------------------------------------ settings page

impl App {
    fn settings_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
            let w = ui.available_width();
            let (rect, _) = ui.allocate_exact_size(vec2(w, 40.0), Sense::hover());
            let p = ui.painter().clone();
            let tg = one_line(&p, "Settings", semi(17.0), TEXT, 200.0);
            p.galley(rect.center() - tg.size() / 2.0, tg, TEXT);
            let link = |id: &str, text: &str, font: FontId, left: bool| -> bool {
                let g = one_line(&p, text, font, ACCENT, 120.0);
                let size = g.size() + vec2(16.0, 12.0);
                let r = if left { Rect::from_min_size(pos2(rect.min.x, rect.center().y - size.y / 2.0), size) } else { Rect::from_min_size(pos2(rect.max.x - size.x, rect.center().y - size.y / 2.0), size) };
                let resp = ui.interact(r, ui.scope_id().with(id), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
                let col = if resp.is_pointer_button_down_on() { ACCENT.gamma_multiply(0.5) } else { ACCENT };
                p.galley(r.center() - g.size() / 2.0, g, col);
                resp.clicked()
            };
            if link("cancel", "Cancel", reg(16.0), true) {
                self.show_settings = false;
            }
            if link("done", "Done", semi(16.0), false) {
                let mode = self.settings.mode;
                self.settings = self.draft.clone();
                self.settings.mode = mode;
                self.show_settings = false;
                self.restart(ctx);
            }
        });
        egui::ScrollArea::vertical().id_salt("settings-scroll").auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 8, bottom: 24 }).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 0.0);
                self.settings_groups(ui, ctx);
            });
        });
    }

    fn settings_groups(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let _ = ctx; // used by the platform-specific action rows
        let d = &mut self.draft;
        let devices = &self.devices;
        match d.mode {
            Mode::Receive => {
                group(ui, "Output", "", |ui| {
                    let label = d.out_device.clone().unwrap_or_else(|| "System default".into());
                    combo_row(ui, Glyph::Speaker, ACCENT, "Play on", "out", label, |ui| {
                        ui.selectable_value(&mut d.out_device, None, "System default");
                        for n in &devices.outputs {
                            ui.selectable_value(&mut d.out_device, Some(n.clone()), n);
                        }
                    });
                });
                group(ui, "Latency", "Lower = less delay. Raise these if you hear crackles.", |ui| {
                    slider_row(ui, "Minimum buffer", &format!("{:.1} ms", d.min_latency), &mut d.min_latency, 0.0..=40.0, 0.5);
                    chips_row(ui, "Device buffer (frames)", "buf", &mut d.buffer, &[(16, "16"), (32, "32"), (64, "64"), (128, "128"), (256, "256")]);
                    row(ui, None, "Lowest latency", TEXT, Some(("Forgets network hiccups after ~4 s instead of ~20 s. May glitch more.", DIM)), Trail::Switch(&mut d.lowest_latency));
                });
                group(ui, "Microphone", "Voice focus keeps voices and removes keyboard, fans and room noise, and evens out your level. Adds about 20 ms to the mic only.", |ui| {
                    let label = d.mic_device.clone().unwrap_or_else(|| "System default".into());
                    combo_row(ui, Glyph::Mic, OK, "Mic", "mic", label, |ui| {
                        ui.selectable_value(&mut d.mic_device, None, "System default");
                        for n in &devices.inputs {
                            ui.selectable_value(&mut d.mic_device, Some(n.clone()), n);
                        }
                    });
                    row(ui, Some((Glyph::Waves, PURPLE)), "Voice focus", TEXT, None, Trail::Switch(&mut d.voice_focus));
                });
                group(ui, "Sound alerts", "Runs Google's YAMNet sound model on this Mac (about 2% of one CPU core). Audio never leaves the Mac; only the alert text goes to the PC.", |ui| {
                    row(ui, Some((Glyph::Bell, BAD)), "Listen for important sounds", TEXT, None, Trail::Switch(&mut d.aware));
                    egui::Frame::new().inner_margin(egui::Margin { left: 14, right: 14, top: 4, bottom: 12 }).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                        ui.horizontal_wrapped(|ui| {
                            for cat in crate::aware::Category::ALL {
                                let on = d.aware_categories.contains(&cat);
                                if chip(ui, cat.label(), on).clicked() {
                                    d.aware_categories.retain(|c| *c != cat);
                                    if !on {
                                        d.aware_categories.push(cat);
                                    }
                                }
                            }
                        });
                    });
                    let sens = match d.aware_sensitivity {
                        s if s < 0.34 => "Fewer alerts",
                        s if s > 0.66 => "More alerts",
                        _ => "Balanced",
                    };
                    let mut s = d.aware_sensitivity as f64;
                    if slider_row(ui, "Sensitivity", sens, &mut s, 0.0..=1.0, 0.0).changed() {
                        d.aware_sensitivity = s as f32;
                    }
                    row(ui, None, "Let me hear it", TEXT, Some(("Ducks the game and mixes the room in for a few seconds", DIM)), Trail::Switch(&mut d.aware_hear));
                    row(ui, None, "Show it on the PC screen", TEXT, None, Trail::Switch(&mut d.aware_pc));
                    row(ui, None, "Mac notification", TEXT, None, Trail::Switch(&mut d.aware_notify));
                });
                group(ui, "Network", "", |ui| {
                    text_row(ui, "Name", &mut d.rx_name, &crate::net::hostname());
                    row(ui, None, "Use two links at once", TEXT, Some(("With a cable and Wi-Fi both connected, every packet goes both ways and the first to arrive wins.", DIM)), Trail::Switch(&mut d.multipath));
                    #[cfg(target_os = "macos")]
                    if row(ui, None, "Stop AirDrop Wi-Fi hopping", ACCENT, Some(("AirDrop makes the Wi-Fi radio hop channels about once a second, causing latency spikes. Lasts until restart.", DIM)), Trail::None).clicked() {
                        let msg = match crate::net::disable_awdl() {
                            Ok(()) => "AirDrop radio off until restart — fewer Wi-Fi spikes".to_string(),
                            Err(e) => format!("Not changed: {e:#}"),
                        };
                        self.toast = Some((msg, ctx.input(|i| i.time)));
                    }
                });
            }
            Mode::Send => {
                group(ui, "Capture", "", |ui| {
                    let label = match &d.source {
                        SourceSel::Loopback(None) => "Everything this PC plays".to_string(),
                        SourceSel::Loopback(Some(n)) => format!("Playback: {n}"),
                        SourceSel::Input(None) => "Default input".into(),
                        SourceSel::Input(Some(n)) => format!("Input: {n}"),
                        SourceSel::Tone => "Test tone".into(),
                    };
                    combo_row(ui, Glyph::Desktop, ACCENT, "Source", "src", label, |ui| {
                        if cfg!(windows) {
                            ui.selectable_value(&mut d.source, SourceSel::Loopback(None), "Everything this PC plays");
                            for n in &devices.outputs {
                                ui.selectable_value(&mut d.source, SourceSel::Loopback(Some(n.clone())), format!("Playback: {n}"));
                            }
                        }
                        for n in &devices.inputs {
                            ui.selectable_value(&mut d.source, SourceSel::Input(Some(n.clone())), format!("Input: {n}"));
                        }
                        ui.selectable_value(&mut d.source, SourceSel::Tone, "Test tone");
                    });
                });
                group(ui, "Quality", "Auto = 24-bit lossless, falling back to Opus only for a device whose link drops audio.", |ui| {
                    chips_row(ui, "Format", "quality", &mut d.quality, &[(CodecChoice::Auto, "Auto"), (CodecChoice::Pcm24, "24-bit"), (CodecChoice::Pcm16, "16-bit"), (CodecChoice::Opus, "Opus")]);
                    let mut fec = d.fec as f64;
                    let fec_text = if d.fec == 0 { "Off".to_string() } else { format!("{}×", d.fec) };
                    if slider_row(ui, "Loss protection", &fec_text, &mut fec, 0.0..=4.0, 1.0).changed() {
                        d.fec = fec as usize;
                    }
                    slider_row(ui, "Packet size", &format!("{:.1} ms", d.frame_ms), &mut d.frame_ms, 0.5..=5.0, 0.5);
                });
                group(ui, "Mac mic", "Where the Mac's mic plays on this PC. Apps then use the matching “CABLE Output” as their microphone.", |ui| {
                    let label = d.mic_out.clone().unwrap_or_else(|| "Auto (VB-Cable)".into());
                    combo_row(ui, Glyph::Mic, OK, "Plays into", "micout", label, |ui| {
                        ui.selectable_value(&mut d.mic_out, None, "Auto (VB-Audio CABLE Input)");
                        for n in &devices.outputs {
                            ui.selectable_value(&mut d.mic_out, Some(n.clone()), n);
                        }
                    });
                });
                group(ui, "Network", "", |ui| {
                    text_row(ui, "Receiver", &mut d.to, "Automatic");
                    row(ui, None, "Use two links at once", TEXT, Some(("With a cable and Wi-Fi both connected, every packet goes both ways and the first to arrive wins.", DIM)), Trail::Switch(&mut d.multipath));
                    #[cfg(windows)]
                    if row(ui, None, "Allow lagless on all networks", ACCENT, Some(("Windows treats a direct cable as a Public network. Fixes that in one admin prompt; needed for the mic over a cable.", DIM)), Trail::None).clicked() {
                        let msg = match crate::net::open_firewall() {
                            Ok(()) => "Firewall updated — lagless works over a direct cable now".to_string(),
                            Err(e) => format!("Firewall not changed: {e:#}"),
                        };
                        self.toast = Some((msg, ctx.input(|i| i.time)));
                    }
                });
                if cfg!(windows) {
                    group(ui, "Windows", "", |ui| {
                        row(ui, Some((Glyph::Speaker, GRAY)), "Follow the Windows volume", TEXT, Some(("The volume dial, keys and mute also set the level on every device. Turn off if you mute the PC to silence its own speakers.", DIM)), Trail::Switch(&mut d.follow_windows_volume));
                        let st = &self.status;
                        let sub = if st.engine_ms > 0.0 { format!("Active: {:.1} ms instead of {:.1} ms per chunk", st.engine_ms, st.engine_default_ms) } else { "Asks Windows for its smallest audio period (often 2.7 ms instead of 10). Not every driver allows it.".into() };
                        row(ui, Some((Glyph::Bolt, OK)), "Speed up the audio engine", TEXT, Some((sub.as_str(), DIM)), Trail::Switch(&mut d.engine_boost));
                    });
                }
            }
        }
        group(ui, "Updates", "", |ui| {
            row(ui, Some((Glyph::Download, GRAY)), "Update automatically", TEXT, None, Trail::Switch(&mut d.auto_update));
            let state = match self.updater.state() {
                update::State::Checking => "Checking…".to_string(),
                update::State::Downloading(v) => format!("Getting {v}…"),
                update::State::Ready(v) => format!("{v} ready"),
                update::State::Available(v) => format!("{v} available"),
                _ => format!("You have {}", update::VERSION),
            };
            if row(ui, None, "Check now", ACCENT, None, Trail::Value(state, DIM)).clicked() {
                self.updater.check_now();
            }
        });
    }
}

// ------------------------------------------------------------------ widgets

const SEG_ON_BG: Color32 = Color32::from_rgb(28, 28, 31);
const SEG_ON_GROUP: Color32 = Color32::from_rgb(48, 48, 52);

fn black(a: f32) -> Color32 {
    Color32::from_rgba_premultiplied(0, 0, 0, (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn reg(size: f32) -> FontId {
    FontId::proportional(size)
}

fn semi(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semi".into()))
}

fn light(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("light".into()))
}

/// Inter (SIL OFL, assets/fonts) in three weights, with egui's fonts behind it for symbols.
fn install_fonts(ctx: &egui::Context) {
    let mut f = egui::FontDefinitions::default();
    let faces: [(&str, &'static [u8]); 3] = [
        ("inter", include_bytes!("../assets/fonts/Inter-Regular.ttf")),
        ("inter-semi", include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
        ("inter-light", include_bytes!("../assets/fonts/InterDisplay-Light.ttf")),
    ];
    for (name, bytes) in faces {
        f.font_data.insert(name.into(), Arc::new(egui::FontData::from_static(bytes)));
    }
    let fallback = f.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    f.families.entry(FontFamily::Proportional).or_default().insert(0, "inter".into());
    for (family, face) in [("semi", "inter-semi"), ("light", "inter-light")] {
        let mut v = vec![face.to_string()];
        v.extend(fallback.iter().cloned());
        f.families.insert(FontFamily::Name(family.into()), v);
    }
    ctx.set_fonts(f);
}

fn style(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = RAISED;
    v.window_stroke = Stroke::new(1.0, Color32::from_white_alpha(18));
    v.window_corner_radius = CornerRadius::same(12);
    v.menu_corner_radius = CornerRadius::same(12);
    v.extreme_bg_color = RAISED;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, TEXT);
    v.hyperlink_color = ACCENT;
    v.text_cursor.stroke = Stroke::new(2.0, ACCENT);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.corner_radius = CornerRadius::same(8);
        w.bg_stroke = Stroke::NONE;
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, SEP);
    v.widgets.inactive.weak_bg_fill = RAISED;
    v.widgets.inactive.bg_fill = RAISED;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, DIM);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(58, 58, 60);
    v.widgets.hovered.bg_fill = Color32::from_rgb(58, 58, 60);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(72, 72, 74);
    v.widgets.open.weak_bg_fill = Color32::from_rgb(58, 58, 60);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 6.0);
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.scroll = egui::style::ScrollStyle::floating();
        s.text_styles.insert(egui::TextStyle::Body, reg(14.0));
        s.text_styles.insert(egui::TextStyle::Button, reg(14.0));
        s.animation_time = 0.18;
    });
}

/// One line of text, cut with "…" if it doesn't fit.
fn one_line(p: &egui::Painter, s: &str, font: FontId, col: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(s.to_string(), font, col);
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(10.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    p.layout_job(job)
}

fn caption(ui: &mut Ui, s: &str) {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 0)).show(ui, |ui| {
        ui.label(RichText::new(s).font(reg(12.5)).color(DIM));
    });
}

/// iOS inset grouped list: caption above, rounded card, optional footnote below.
fn group<R>(ui: &mut Ui, header: &str, footer: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    if !header.is_empty() {
        egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 7 }).show(ui, |ui| {
            ui.label(RichText::new(header.to_uppercase()).font(reg(12.0)).color(DIM));
        });
    }
    let r = egui::Frame::new()
        .fill(GROUP)
        .corner_radius(CornerRadius::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner;
    if !footer.is_empty() {
        ui.add_space(7.0);
        caption(ui, footer);
    }
    ui.add_space(24.0);
    r
}

fn is_first(ui: &Ui) -> bool {
    ui.cursor().min.y <= ui.max_rect().min.y + 0.5
}

fn separator(ui: &Ui, y: f32, x0: f32) {
    let px = 1.0 / ui.ctx().pixels_per_point().max(1.0);
    ui.painter().line_segment([pos2(x0, y), pos2(ui.max_rect().max.x, y)], Stroke::new(px.max(0.5), SEP));
}

enum Trail<'a> {
    None,
    Switch(&'a mut bool),
    /// Included (filled check) or not (empty circle).
    Check(bool),
    Value(String, Color32),
    Chevron(String),
    /// Expand/collapse chevron.
    Disclose(bool),
}

/// A list row: optional colored icon tile, title, optional subtitle, and a trailing control.
fn row(ui: &mut Ui, icon: Option<(Glyph, Color32)>, title: &str, tcol: Color32, sub: Option<(&str, Color32)>, mut trail: Trail) -> egui::Response {
    let first = is_first(ui);
    let w = ui.available_width();
    let p = ui.painter().clone();
    let lead = if icon.is_some() { 58.0 } else { 16.0 };
    let tg = match &trail {
        Trail::Value(s, c) => Some(one_line(&p, s, reg(15.0), *c, w * 0.55)),
        Trail::Chevron(s) if !s.is_empty() => Some(one_line(&p, s, reg(15.0), DIM, w * 0.45)),
        _ => None,
    };
    let tgw = tg.as_ref().map_or(0.0, |g| g.size().x);
    let trail_w = match &trail {
        Trail::None => 0.0,
        Trail::Switch(_) => 64.0,
        Trail::Check(_) => 44.0,
        Trail::Disclose(_) => 34.0,
        Trail::Value(..) => tgw + 22.0,
        Trail::Chevron(_) => tgw + 38.0,
    };
    let text_w = (w - lead - trail_w - 6.0).max(40.0);
    let title_g = one_line(&p, title, reg(15.0), tcol, text_w);
    let sub_g = sub.map(|(s, c)| p.layout(s.to_string(), reg(12.5), c, text_w));
    let content_h = title_g.size().y + sub_g.as_ref().map_or(0.0, |g| g.size().y + 2.0);
    let h = (content_h + 24.0).max(46.0);
    let clickable = !matches!(trail, Trail::None | Trail::Value(..)) || tcol == ACCENT;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, h), if clickable { Sense::click() } else { Sense::hover() });
    if clickable {
        resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let a = if resp.is_pointer_button_down_on() { 22 } else if resp.hovered() { 9 } else { 0 };
        if a > 0 {
            p.rect_filled(rect.shrink2(vec2(5.0, 3.0)), 10.0, Color32::from_white_alpha(a));
        }
    }
    if !first {
        separator(ui, rect.min.y, rect.min.x + lead);
    }
    if let Some((g, col)) = icon {
        let tile = Rect::from_center_size(pos2(rect.min.x + 30.0, rect.center().y), vec2(30.0, 30.0));
        p.rect_filled(tile, 8.0, col);
        glyph(&p, tile.center(), g, Color32::WHITE, 0.85);
    }
    let y0 = rect.center().y - content_h / 2.0;
    let th = title_g.size().y;
    p.galley(pos2(rect.min.x + lead, y0), title_g, tcol);
    if let Some(g) = sub_g {
        p.galley(pos2(rect.min.x + lead, y0 + th + 2.0), g, DIM);
    }
    let rx = rect.max.x - 14.0;
    let cy = rect.center().y;
    match &mut trail {
        Trail::None => {}
        Trail::Switch(on) => {
            if resp.clicked() {
                **on = !**on;
                resp.mark_changed();
            }
            let t = ui.ctx().animate_bool_responsive(resp.id, **on);
            paint_switch(&p, Rect::from_center_size(pos2(rx - 23.0, cy), vec2(48.0, 29.0)), t);
        }
        Trail::Check(on) => {
            let t = ui.ctx().animate_bool_responsive(resp.id, *on);
            let c = pos2(rx - 11.0, cy);
            p.circle_stroke(c, 10.5, Stroke::new(1.5, GRAY.gamma_multiply(1.0 - t)));
            if t > 0.0 {
                p.circle_filled(c, 11.0 * (0.6 + 0.4 * t), ACCENT.gamma_multiply(t));
                checkmark(&p, c, Color32::WHITE.gamma_multiply(t));
            }
        }
        Trail::Value(_, c) => {
            if let Some(g) = tg {
                p.galley(pos2(rx - g.size().x, cy - g.size().y / 2.0), g, *c);
            }
        }
        Trail::Chevron(_) => {
            chevron(&p, pos2(rx - 4.0, cy), 0.0, GRAY);
            if let Some(g) = tg {
                p.galley(pos2(rx - 18.0 - g.size().x, cy - g.size().y / 2.0), g, DIM);
            }
        }
        Trail::Disclose(open) => {
            let t = ui.ctx().animate_bool_responsive(resp.id, *open);
            chevron(&p, pos2(rx - 6.0, cy), t * std::f32::consts::FRAC_PI_2, GRAY);
        }
    }
    resp
}

fn paint_switch(p: &egui::Painter, r: Rect, t: f32) {
    let track = TRACK.lerp_to_gamma(GOOD, t);
    p.rect_filled(r, r.height() / 2.0, track);
    let rad = r.height() / 2.0 - 2.0;
    let x = egui::lerp((r.min.x + 2.0 + rad)..=(r.max.x - 2.0 - rad), t);
    let c = pos2(x, r.center().y);
    p.circle_filled(c + vec2(0.0, 1.5), rad + 0.5, black(0.25));
    p.circle_filled(c, rad, Color32::WHITE);
}

/// Soft radial gradient (a triangle fan with a mid ring, so it falls off smoothly, no banding).
fn glow(p: &egui::Painter, c: Pos2, r: f32, col: Color32) {
    let mut m = egui::Mesh::default();
    m.colored_vertex(c, col);
    const N: u32 = 64;
    for i in 0..N {
        let a = i as f32 / N as f32 * std::f32::consts::TAU;
        let d = vec2(a.cos(), a.sin());
        m.colored_vertex(c + d * r * 0.45, col.gamma_multiply(0.45));
        m.colored_vertex(c + d * r, Color32::TRANSPARENT);
    }
    for i in 0..N {
        let (a, b) = (1 + 2 * i, 1 + 2 * ((i + 1) % N));
        m.add_triangle(0, a, b);
        m.add_triangle(a, a + 1, b + 1);
        m.add_triangle(a, b + 1, b);
    }
    p.add(m);
}

fn checkmark(p: &egui::Painter, c: Pos2, col: Color32) {
    let pts = vec![c + vec2(-4.8, 0.2), c + vec2(-1.6, 3.4), c + vec2(4.8, -3.4)];
    p.add(egui::Shape::line(pts, Stroke::new(2.0, col)));
}

/// "›", rotated by `angle` (radians, clockwise).
fn chevron(p: &egui::Painter, c: Pos2, angle: f32, col: Color32) {
    let rot = egui::emath::Rot2::from_angle(angle);
    let pts = [vec2(-2.5, -5.5), vec2(3.0, 0.0), vec2(-2.5, 5.5)].iter().map(|v| c + rot * *v).collect();
    p.add(egui::Shape::line(pts, Stroke::new(2.0, col)));
}

/// Row with a dropdown on the right.
fn combo_row(ui: &mut Ui, icon: Glyph, col: Color32, title: &str, id: &str, selected: String, add: impl FnOnce(&mut Ui)) {
    let first = is_first(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 50.0), Sense::hover());
    let p = ui.painter().clone();
    if !first {
        separator(ui, rect.min.y, rect.min.x + 58.0);
    }
    let tile = Rect::from_center_size(pos2(rect.min.x + 30.0, rect.center().y), vec2(30.0, 30.0));
    p.rect_filled(tile, 8.0, col);
    glyph(&p, tile.center(), icon, Color32::WHITE, 0.85);
    let tg = one_line(&p, title, reg(15.0), TEXT, 120.0);
    let tw = tg.size().x;
    p.galley(pos2(rect.min.x + 58.0, rect.center().y - tg.size().y / 2.0), tg, TEXT);
    let cw = (w - 58.0 - tw - 30.0).clamp(80.0, 260.0);
    let cr = Rect::from_min_size(pos2(rect.max.x - 10.0 - cw, rect.center().y - 15.0), vec2(cw, 30.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(cr).layout(Layout::right_to_left(Align::Center)));
    egui::ComboBox::from_id_salt(id).width(cw).height(320.0).truncate().selected_text(RichText::new(selected).color(TEXT)).show_ui(&mut child, add);
}

/// Title and value on one line, slider under it.
fn slider_row(ui: &mut Ui, title: &str, value: &str, v: &mut f64, range: std::ops::RangeInclusive<f64>, step: f64) -> egui::Response {
    let first = is_first(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    if !first {
        separator(ui, rect.min.y, rect.min.x + 16.0);
    }
    let p = ui.painter().clone();
    let tg = one_line(&p, title, reg(15.0), TEXT, w * 0.6);
    p.galley(pos2(rect.min.x + 16.0, rect.max.y - tg.size().y - 2.0), tg, TEXT);
    let vg = one_line(&p, value, reg(15.0), DIM, w * 0.35);
    p.galley(pos2(rect.max.x - 16.0 - vg.size().x, rect.max.y - vg.size().y - 2.0), vg, DIM);
    egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 8, top: 2, bottom: 8 }).show(ui, |ui| slider(ui, v, range, step)).inner
}

fn slider(ui: &mut Ui, v: &mut f64, range: std::ops::RangeInclusive<f64>, step: f64) -> egui::Response {
    let w = ui.available_width();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click_and_drag());
    let resp_h = resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand);
    let (lo, hi) = (*range.start(), *range.end());
    let (x0, x1) = (rect.min.x + 14.0, rect.max.x - 14.0);
    if let Some(pos) = resp_h.interact_pointer_pos() {
        if resp.dragged() || resp.clicked() || resp.is_pointer_button_down_on() {
            let t = ((pos.x - x0) / (x1 - x0)).clamp(0.0, 1.0) as f64;
            let mut nv = lo + (hi - lo) * t;
            if step > 0.0 {
                nv = (nv / step).round() * step;
            }
            nv = nv.clamp(lo, hi);
            if (nv - *v).abs() > 1e-9 {
                *v = nv;
                resp.mark_changed();
            }
        }
    }
    let t = (((*v - lo) / (hi - lo)) as f32).clamp(0.0, 1.0);
    let shown = if resp.dragged() { t } else { ui.ctx().animate_value_with_time(resp.id, t, 0.1) };
    let kx = egui::lerp(x0..=x1, shown);
    let cy = rect.center().y;
    let p = ui.painter();
    p.rect_filled(Rect::from_min_max(pos2(x0, cy - 2.0), pos2(x1, cy + 2.0)), 2.0, TRACK);
    p.rect_filled(Rect::from_min_max(pos2(x0, cy - 2.0), pos2(kx, cy + 2.0)), 2.0, ACCENT);
    p.circle_filled(pos2(kx, cy + 1.5), 13.0, black(0.3));
    p.circle_filled(pos2(kx, cy), 12.5, Color32::WHITE);
    resp
}

/// Title, with a segmented control under it.
fn chips_row<T: PartialEq + Copy>(ui: &mut Ui, title: &str, id: &str, v: &mut T, options: &[(T, &str)]) {
    let first = is_first(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
    if !first {
        separator(ui, rect.min.y, rect.min.x + 16.0);
    }
    let p = ui.painter().clone();
    let tg = one_line(&p, title, reg(15.0), TEXT, w - 32.0);
    p.galley(pos2(rect.min.x + 16.0, rect.max.y - tg.size().y - 2.0), tg, TEXT);
    egui::Frame::new().inner_margin(egui::Margin { left: 14, right: 14, top: 8, bottom: 12 }).show(ui, |ui| {
        if let Some(n) = segmented(ui, id, *v, options, 30.0, SEG_ON_GROUP) {
            *v = n;
        }
    });
}

/// iOS segmented control; the highlight slides to the new choice.
fn segmented<T: PartialEq + Copy>(ui: &mut Ui, id: &str, value: T, options: &[(T, &str)], h: f32, track: Color32) -> Option<T> {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter().clone();
    p.rect_filled(rect, 9.0, track);
    let n = options.len().max(1) as f32;
    let seg = w / n;
    let idx = options.iter().position(|(v, _)| *v == value).unwrap_or(0) as f32;
    let base = ui.scope_id().with(id);
    let x = ui.ctx().animate_value_with_time(base, idx, 0.24);
    let hl = Rect::from_min_size(pos2(rect.min.x + 2.0 + x * seg, rect.min.y + 2.0), vec2(seg - 4.0, h - 4.0));
    p.rect_filled(hl.translate(vec2(0.0, 1.0)), 7.0, black(0.25));
    p.rect_filled(hl, 7.0, Color32::from_rgb(99, 99, 102));
    let mut out = None;
    for (i, (v, label)) in options.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.min.x + i as f32 * seg, rect.min.y), vec2(seg, h));
        let resp = ui.interact(r, base.with(i), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = (i as f32 - x).abs() < 0.5;
        let g = one_line(&p, label, if on { semi(13.0) } else { reg(13.0) }, TEXT, seg - 8.0);
        p.galley(r.center() - g.size() / 2.0, g, TEXT);
        if resp.clicked() && *v != value {
            out = Some(*v);
        }
    }
    out
}

/// Pill-shaped toggle chip (multi-select).
fn chip(ui: &mut Ui, label: &str, on: bool) -> egui::Response {
    let p = ui.painter().clone();
    let g = one_line(&p, label, reg(13.0), TEXT, 200.0);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(26.0, 14.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let t = ui.ctx().animate_bool_responsive(resp.id, on);
    let fill = SEG_ON_GROUP.lerp_to_gamma(ACCENT, t);
    p.rect_filled(rect, rect.height() / 2.0, if resp.hovered() { fill.gamma_multiply(1.12) } else { fill });
    p.galley(rect.center() - g.size() / 2.0, g, TEXT);
    resp
}

/// Label on the left, free text on the right.
fn text_row(ui: &mut Ui, title: &str, value: &mut String, hint: &str) {
    let first = is_first(ui);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 46.0), Sense::hover());
    if !first {
        separator(ui, rect.min.y, rect.min.x + 16.0);
    }
    let p = ui.painter().clone();
    let tg = one_line(&p, title, reg(15.0), TEXT, 120.0);
    let tw = tg.size().x;
    p.galley(pos2(rect.min.x + 16.0, rect.center().y - tg.size().y / 2.0), tg, TEXT);
    let ew = w - tw - 48.0;
    let er = Rect::from_min_size(pos2(rect.max.x - 16.0 - ew, rect.center().y - 12.0), vec2(ew, 24.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(er));
    child.add(
        egui::TextEdit::singleline(value)
            .frame(egui::Frame::NONE)
            .font(reg(15.0))
            .text_color(TEXT)
            .align(egui::Align2::RIGHT_CENTER)
            .hint_text(RichText::new(hint).color(GRAY.gamma_multiply(1.4)))
            .desired_width(ew),
    );
}

fn pill(ui: &mut Ui, text: &str, fill: Color32, col: Color32) -> egui::Response {
    let p = ui.painter().clone();
    let g = one_line(&p, text, semi(13.0), col, 200.0);
    let (rect, resp) = ui.allocate_exact_size(g.size() + vec2(28.0, 14.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    p.rect_filled(rect, rect.height() / 2.0, if resp.hovered() { fill.gamma_multiply(1.15) } else { fill });
    p.galley(rect.center() - g.size() / 2.0, g, col);
    resp
}

fn round_button(ui: &mut Ui, g: Glyph) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter();
    p.circle_filled(rect.center(), 18.0, if resp.is_pointer_button_down_on() { RAISED } else if resp.hovered() { Color32::from_rgb(38, 38, 40) } else { GROUP });
    glyph(p, rect.center(), g, if resp.hovered() { TEXT } else { DIM }, 1.0);
    resp
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

/// Notification-style card shown inside the lagless window.
fn alert_banner(ui: &mut Ui, text: &str) {
    egui::Frame::new()
        .fill(RAISED)
        .corner_radius(CornerRadius::same(18))
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            alert_content(ui, text);
        });
}

fn alert_content(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(vec2(38.0, 38.0), Sense::hover());
        ui.painter().rect_filled(tile, 10.0, BAD);
        glyph(ui.painter(), tile.center(), Glyph::Bell, Color32::WHITE, 1.05);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(RichText::new(text).font(semi(16.0)).color(TEXT));
            ui.label(RichText::new("Heard in the room").font(reg(12.5)).color(DIM));
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
                .fill(Color32::from_rgba_unmultiplied(36, 36, 38, 242))
                .stroke(Stroke::new(1.0, Color32::from_white_alpha(22)))
                .corner_radius(CornerRadius::same(20))
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    alert_content(ui, &text);
                });
        });
    });
}

/// Rasterise every glyph the UI uses at every size up front. The alert overlay is a second
/// window sharing the font texture; if new glyphs were added to it while the overlay is up
/// (e.g. the banner's text appearing in the main window), the overlay could draw with a stale copy.
fn warm_fonts(ctx: &egui::Context) {
    const CHARS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789 .,:;!?'\"()[]/-–—…·%+→×°“”";
    let p = egui::Painter::new(ctx.clone(), egui::LayerId::background(), Rect::NOTHING);
    for size in [11.0, 11.5, 12.0, 12.5, 13.0, 13.5, 14.0, 15.0, 16.0, 17.0, 18.0, 22.0, 30.0] {
        for font in [reg(size), semi(size)] {
            let _ = p.layout_no_wrap(CHARS.to_string(), font, TEXT);
        }
    }
    let _ = p.layout_no_wrap("0123456789–".to_string(), light(76.0), TEXT);
}

#[derive(Clone, Copy)]
enum Glyph {
    Mic,
    Bell,
    Speaker,
    Muted,
    Laptop,
    Phone,
    Desktop,
    Sync,
    Bolt,
    Wifi,
    Cable,
    Download,
    Info,
    Waves,
    Sliders,
}

fn device_glyph(name: &str) -> Glyph {
    let n = name.to_lowercase();
    if ["phone", "galaxy", "pixel", "sm-", "android", "ipad", "tab", "oneplus", "xiaomi"].iter().any(|k| n.contains(k)) {
        Glyph::Phone
    } else if ["macbook", "laptop", "book"].iter().any(|k| n.contains(k)) {
        Glyph::Laptop
    } else {
        Glyph::Desktop
    }
}

fn link_glyph(l: crate::engine::LinkKind) -> Glyph {
    use crate::engine::LinkKind::*;
    match l {
        WiFi => Glyph::Wifi,
        Cable | Thunderbolt => Glyph::Cable,
        Bluetooth => Glyph::Waves,
        _ => Glyph::Wifi,
    }
}

/// Painted icons (no icon font): drawn in a ~18 pt box around `c`, scaled by `s`.
fn glyph(p: &egui::Painter, c: Pos2, g: Glyph, col: Color32, s: f32) {
    let v = |x: f32, y: f32| c + vec2(x, y) * s;
    let st = Stroke::new(1.7 * s, col);
    let arc = |center: Pos2, r: f32, a0: f32, a1: f32| -> egui::Shape {
        let pts = (0..=12).map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / 12.0;
            center + vec2(a.cos(), a.sin()) * r * s
        });
        egui::Shape::line(pts.collect(), st)
    };
    use std::f32::consts::PI;
    match g {
        Glyph::Mic => {
            p.rect_filled(Rect::from_min_max(v(-3.6, -8.0), v(3.6, 2.5)), 3.6 * s, col);
            p.add(arc(v(0.0, -1.0), 6.3, 0.0, PI));
            p.line_segment([v(0.0, 5.3), v(0.0, 8.0)], st);
            p.line_segment([v(-3.0, 8.0), v(3.0, 8.0)], st);
        }
        Glyph::Bell => {
            let body = vec![v(-7.0, 4.5), v(-5.5, 2.5), v(-5.0, -2.5), v(-2.5, -6.5), v(2.5, -6.5), v(5.0, -2.5), v(5.5, 2.5), v(7.0, 4.5)];
            p.add(egui::Shape::convex_polygon(body, col, Stroke::NONE));
            p.circle_filled(v(0.0, 7.0), 2.2 * s, col);
            p.circle_filled(v(0.0, -7.3), 1.4 * s, col);
        }
        Glyph::Speaker | Glyph::Muted => {
            let body = vec![v(-8.0, -3.0), v(-4.5, -3.0), v(0.0, -7.0), v(0.0, 7.0), v(-4.5, 3.0), v(-8.0, 3.0)];
            p.add(egui::Shape::convex_polygon(body, col, Stroke::NONE));
            if matches!(g, Glyph::Muted) {
                p.line_segment([v(3.0, -3.5), v(8.5, 3.5)], st);
                p.line_segment([v(8.5, -3.5), v(3.0, 3.5)], st);
            } else {
                p.add(arc(v(0.0, 0.0), 4.5, -0.75, 0.75));
                p.add(arc(v(0.0, 0.0), 8.0, -0.8, 0.8));
            }
        }
        Glyph::Laptop => {
            p.rect_stroke(Rect::from_min_max(v(-6.0, -5.5), v(6.0, 3.0)), 1.5 * s, st, egui::StrokeKind::Middle);
            p.line_segment([v(-8.5, 6.0), v(8.5, 6.0)], Stroke::new(2.2 * s, col));
        }
        Glyph::Phone => {
            p.rect_stroke(Rect::from_min_max(v(-4.5, -8.0), v(4.5, 8.0)), 2.5 * s, st, egui::StrokeKind::Middle);
            p.line_segment([v(-1.5, 5.3), v(1.5, 5.3)], st);
        }
        Glyph::Desktop => {
            p.rect_stroke(Rect::from_min_max(v(-7.5, -6.5), v(7.5, 3.5)), 1.5 * s, st, egui::StrokeKind::Middle);
            p.line_segment([v(0.0, 3.5), v(0.0, 7.0)], st);
            p.line_segment([v(-4.0, 7.0), v(4.0, 7.0)], st);
        }
        Glyph::Sync => {
            p.circle_stroke(v(-3.2, 0.0), 5.0 * s, st);
            p.circle_stroke(v(3.2, 0.0), 5.0 * s, st);
        }
        Glyph::Bolt => {
            p.add(egui::Shape::convex_polygon(vec![v(2.0, -8.5), v(-5.5, 1.5), v(1.0, 1.5)], col, Stroke::NONE));
            p.add(egui::Shape::convex_polygon(vec![v(-1.0, -1.5), v(5.5, -1.5), v(-2.0, 8.5)], col, Stroke::NONE));
        }
        Glyph::Wifi => {
            let base = v(0.0, 6.0);
            for r in [4.5f32, 8.5, 12.5] {
                p.add(arc(base, r, -PI * 0.75, -PI * 0.25));
            }
            p.circle_filled(base, 1.6 * s, col);
        }
        Glyph::Cable => {
            p.rect_filled(Rect::from_min_max(v(-5.0, -6.0), v(5.0, 2.0)), 1.5 * s, col);
            p.line_segment([v(-2.0, -6.0), v(-2.0, -8.5)], st);
            p.line_segment([v(2.0, -6.0), v(2.0, -8.5)], st);
            p.line_segment([v(0.0, 2.0), v(0.0, 8.5)], st);
        }
        Glyph::Download => {
            p.line_segment([v(0.0, -7.5), v(0.0, 3.0)], st);
            p.add(egui::Shape::line(vec![v(-4.0, -1.0), v(0.0, 3.2), v(4.0, -1.0)], st));
            p.line_segment([v(-6.5, 7.0), v(6.5, 7.0)], st);
        }
        Glyph::Info => {
            p.circle_stroke(c, 7.5 * s, st);
            p.circle_filled(v(0.0, -3.6), 1.2 * s, col);
            p.line_segment([v(0.0, -1.0), v(0.0, 4.2)], Stroke::new(1.9 * s, col));
        }
        Glyph::Waves => {
            for (x, h) in [(-6.0f32, 3.0f32), (-3.0, 7.0), (0.0, 10.0), (3.0, 6.0), (6.0, 3.5)] {
                p.line_segment([v(x, -h / 2.0), v(x, h / 2.0)], Stroke::new(1.9 * s, col));
            }
        }
        Glyph::Sliders => {
            for (k, knob) in [0.3f32, 0.7, 0.45].iter().enumerate() {
                let y = -5.5 + k as f32 * 5.5;
                p.line_segment([v(-7.0, y), v(7.0, y)], Stroke::new(1.6 * s, col));
                p.circle_filled(v(-7.0 + 14.0 * knob, y), 2.6 * s, col);
            }
        }
    }
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
            .with_inner_size([400.0, 680.0])
            .with_min_inner_size([360.0, 480.0])
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
