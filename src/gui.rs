//! Desktop app: one window for both ends. Lightweight immediate-mode UI (egui), no web engine.
//! Deliberately quiet: nothing animates while streaming, so it never pulls focus.

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
    fn spawn(name: &str, job: Job, gain: f32, ctx: egui::Context) -> Self {
        let shared = Shared::new();
        shared.volume.set(gain); // before the first sample, so a low setting never blasts
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
        self.engine.stop();
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
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
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
                    self.status_block(ui);
                    self.volume_row(ui);
                    ui.add_space(14.0);
                    self.mic_row(ui, &ctx);
                    ui.add_space(10.0);
                    egui::ScrollArea::vertical().max_height(ui.available_height() - 36.0).show(ui, |ui| self.details(ui));
                }
                self.footer(ui, now, &ctx);
            });

        // Nothing animates, so a few frames a second is plenty for live numbers.
        ctx.request_repaint_after(Duration::from_millis(250));
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
                    (Phase::Idle, _) => ("no mic audio".to_string(), DIM, ""),
                    _ => ("connecting…".to_string(), DIM, ""),
                };
                let l = ui.label(RichText::new(text).size(11.5).color(col));
                if !tip.is_empty() {
                    l.on_hover_text(tip);
                }
            });
        });
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
                if st.target_ms > 0.0 {
                    row(ui, "Buffer", format!("{:.1} ms (target {:.1})", st.buffer_ms, st.target_ms));
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
