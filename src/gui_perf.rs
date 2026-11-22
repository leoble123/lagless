//! Game performance stats in the UI: the card (PC and Mac), the PC's on-screen display, and
//! their settings. Numbers come from perf.rs (PC) or over the network (Mac).

use super::*;
use crate::perf::{PerfSnapshot, NEEDS_PERMISSION};
use std::collections::VecDeque;

/// Frame-time history kept for the graph: 10 s.
const HISTORY: usize = (10_000.0 / crate::perf::GRAPH_SLICE_MS) as usize;

#[derive(Default)]
pub(super) struct PerfView {
    pub snap: PerfSnapshot,
    pub history: VecDeque<f32>,
    pub at: f64,
}

impl PerfView {
    fn update(&mut self, mut s: PerfSnapshot, now: f64) {
        if s.game != self.snap.game {
            self.history.clear();
        }
        self.history.extend(s.graph.drain(..));
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.snap = s;
        self.at = now;
    }

    fn fresh(&self, now: f64) -> bool {
        self.at > 0.0 && now - self.at < 3.0
    }

    fn has_game(&self, now: f64) -> bool {
        self.fresh(now) && !self.snap.game.is_empty()
    }
}

impl App {
    pub(super) fn restart_perf(&mut self, ctx: &egui::Context) {
        self.perf = None;
        self.perf_view = PerfView::default();
        if self.settings.mode == Mode::Send && self.settings.perf_stats {
            let wake = ctx.clone();
            self.perf = Some(crate::perf::Monitor::start(move || wake.request_repaint()));
        }
    }

    /// Runs every frame (also while the window is in the background): move fresh stats to the
    /// card, the Macs and the on-screen display.
    pub(super) fn pump_perf(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if let Some(m) = &self.perf {
            if let Some(s) = m.take_fresh() {
                self.engine.shared.send_perf(s.to_json());
                self.perf_view.update(s, now);
            }
            if self.settings.perf_osd && self.perf_view.has_game(now) {
                perf_osd(ctx, &self.perf_view, self.settings.perf_osd_corner);
            }
            ctx.request_repaint_after(Duration::from_millis(250));
        } else {
            for json in self.engine.shared.take_perf() {
                if let Some(s) = PerfSnapshot::from_json(&json) {
                    self.perf_view.update(s, now);
                }
            }
        }
    }

    /// The stats card on the main page. PC: while stats are on. Mac: while a game is running.
    pub(super) fn perf_card(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let sending = self.settings.mode == Mode::Send;
        let v = &self.perf_view;
        let show = if sending { self.perf.is_some() } else { self.settings.perf_show && v.has_game(now) };
        if !show {
            return;
        }
        let s = v.snap.clone();
        let live = v.has_game(now);
        let header = if live { format!("Game · {}", s.game) } else { "Game".to_string() };
        let mut allow = false;
        group(ui, &header, "", |ui| {
            if !live {
                let (title, sub): (&str, String) = if s.fps_problem == NEEDS_PERMISSION {
                    ("FPS counter needs permission", "Windows only lets admins read frame timing. One prompt fixes it for good.".into())
                } else if !s.fps_problem.is_empty() {
                    ("No FPS", s.fps_problem.clone())
                } else {
                    ("No game running", "Start one in windowed fullscreen (borderless) and the stats show up here and on your Mac".into())
                };
                row(ui, Some((Glyph::Gauge, GRAY)), title, TEXT, Some((sub.as_str(), DIM)), Trail::None);
                if s.fps_problem == NEEDS_PERMISSION && row(ui, None, "Allow the FPS counter", ACCENT, None, Trail::None).clicked() {
                    allow = true;
                }
                hardware_rows(ui, &s);
                return;
            }
            fps_block(ui, &s);
            egui::Frame::new().inner_margin(egui::Margin { left: 14, right: 14, top: 0, bottom: 10 }).show(ui, |ui| frame_graph(ui, &v.history, 64.0));
            if !s.verdict.is_empty() {
                let col = verdict_color(&s.verdict);
                row(ui, Some((Glyph::Gauge, col)), &s.verdict, TEXT, Some((s.tip.as_str(), DIM)), Trail::None);
            }
            hardware_rows(ui, &s);
            if s.stutters > 0 {
                let title = if s.stutters == 1 { "1 stutter".to_string() } else { format!("{} stutters", s.stutters) };
                row(ui, Some((Glyph::Bolt, OK)), &title, TEXT, Some((s.last_stutter.as_str(), DIM)), Trail::None);
            }
        });
        if allow {
            #[cfg(windows)]
            {
                let msg = match crate::perf::allow_fps_counter() {
                    Ok(()) => "Done. Sign out of Windows and back in once, then FPS shows up".to_string(),
                    Err(e) => format!("Not changed: {e:#}"),
                };
                self.toast = Some((msg, now));
            }
        }
    }

    /// Settings → Performance (PC) / PC performance (Mac).
    pub(super) fn perf_settings(d: &mut Settings, ui: &mut Ui) {
        match d.mode {
            Mode::Send => {
                group(ui, "Game stats", "Reads frame timing from Windows itself (like PresentMon), NVIDIA's driver and Task Manager's counters. Nothing touches the game, so anti-cheat has nothing to see. The on-screen display shows over games in windowed fullscreen (borderless), not exclusive fullscreen.", |ui| {
                    row(ui, Some((Glyph::Gauge, GOOD)), "FPS & performance stats", TEXT, Some(("On this PC and on your Mac", DIM)), Trail::Switch(&mut d.perf_stats));
                    row(ui, None, "On-screen display", TEXT, Some(("A small click-through counter over the game", DIM)), Trail::Switch(&mut d.perf_osd));
                    chips_row(ui, "Display corner", "osd-corner", &mut d.perf_osd_corner, &[(0, "Top left"), (1, "Top right"), (2, "Bot. left"), (3, "Bot. right")]);
                });
            }
            Mode::Receive => {
                group(ui, "PC performance", "", |ui| {
                    row(ui, Some((Glyph::Gauge, GOOD)), "Show the PC's game stats", TEXT, Some(("FPS, frame times, GPU and CPU while a game runs", DIM)), Trail::Switch(&mut d.perf_show));
                });
            }
        }
    }
}

fn verdict_color(v: &str) -> Color32 {
    if v.starts_with("GPU") {
        ACCENT
    } else if v.starts_with("CPU") {
        OK
    } else {
        GOOD
    }
}

fn fps_color(fps: f32) -> Color32 {
    if fps >= 120.0 {
        GOOD
    } else if fps >= 60.0 {
        OK
    } else {
        BAD
    }
}

/// Big FPS number, with the lows and frame time beside it.
fn fps_block(ui: &mut Ui, s: &PerfSnapshot) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 78.0), Sense::hover());
    let p = ui.painter().clone();
    let g = one_line(&p, &format!("{:.0}", s.fps), light(52.0), TEXT, w);
    let gs = g.size();
    let x = rect.min.x + 16.0;
    let y = rect.center().y - gs.y / 2.0 + 2.0;
    p.galley(pos2(x, y), g, TEXT);
    let u = one_line(&p, "FPS", semi(13.0), fps_color(s.fps), 60.0);
    p.galley(pos2(x + gs.x + 6.0, y + gs.y - u.size().y - 11.0), u, TEXT);
    let col_x = rect.max.x - 16.0;
    let lines = [
        ("1% low", if s.low1 > 0.0 { format!("{:.0}", s.low1) } else { "–".into() }),
        ("0.1% low", if s.low01 > 0.0 { format!("{:.0}", s.low01) } else { "–".into() }),
        ("Frame time", format!("{:.1} ms", s.frame_ms)),
    ];
    for (i, (k, val)) in lines.iter().enumerate() {
        let ly = rect.min.y + 14.0 + i as f32 * 19.0;
        let vg = one_line(&p, val, semi(13.0), TEXT, 90.0);
        let vw = vg.size().x;
        p.galley(pos2(col_x - vw, ly), vg, TEXT);
        let kg = one_line(&p, k, reg(12.5), DIM, 90.0);
        p.galley(pos2(col_x - 64.0 - kg.size().x, ly), kg, DIM);
    }
}

/// Frame-time graph: one point per 50 ms (the worst frame in it), spikes in red.
fn frame_graph(ui: &mut Ui, history: &VecDeque<f32>, h: f32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    paint_graph(ui.painter(), rect, history, true);
}

fn paint_graph(p: &egui::Painter, rect: Rect, history: &VecDeque<f32>, labels: bool) {
    p.rect_filled(rect, 8.0, Color32::from_white_alpha(6));
    if history.len() < 2 {
        return;
    }
    let mut sorted: Vec<f32> = history.iter().copied().collect();
    sorted.sort_by(f32::total_cmp);
    let median = sorted[sorted.len() / 2];
    let top = (median * 3.0).max(12.0);
    let n = HISTORY as f32;
    let inner = rect.shrink2(vec2(6.0, 6.0));
    let x_of = |i: usize| inner.max.x - (history.len() - 1 - i) as f32 / (n - 1.0) * inner.width();
    let y_of = |v: f32| inner.max.y - (v / top).min(1.0) * inner.height();
    // Median guide.
    let my = y_of(median);
    p.line_segment([pos2(inner.min.x, my), pos2(inner.max.x, my)], Stroke::new(1.0, Color32::from_white_alpha(14)));
    // Soft fill under the line, then the line.
    let pts: Vec<Pos2> = history.iter().enumerate().map(|(i, v)| pos2(x_of(i), y_of(*v))).collect();
    let mut mesh = egui::Mesh::default();
    for pt in &pts {
        mesh.colored_vertex(*pt, ACCENT.gamma_multiply(0.22));
        mesh.colored_vertex(pos2(pt.x, inner.max.y), ACCENT.gamma_multiply(0.0));
    }
    for i in 0..pts.len().saturating_sub(1) as u32 {
        let a = i * 2;
        mesh.add_triangle(a, a + 1, a + 2);
        mesh.add_triangle(a + 1, a + 3, a + 2);
    }
    p.add(mesh);
    p.add(egui::Shape::line(pts.clone(), Stroke::new(1.6, ACCENT)));
    let spike = (median * 2.5).max(25.0);
    for (i, v) in history.iter().enumerate() {
        if *v > spike {
            p.circle_filled(pts[i], 3.0, BAD);
        }
    }
    if labels {
        let g = one_line(p, &format!("{:.0} ms", top), reg(10.5), DIM.gamma_multiply(0.7), 60.0);
        p.galley(pos2(inner.min.x + 2.0, rect.min.y + 3.0), g, DIM);
    }
}

fn opt(v: Option<f32>, f: impl Fn(f32) -> String) -> Option<String> {
    v.map(f)
}

/// GPU, VRAM, CPU, RAM as value rows.
fn hardware_rows(ui: &mut Ui, s: &PerfSnapshot) {
    let gpu: Vec<String> = [opt(s.gpu_util, |u| format!("{u:.0}%")), opt(s.gpu_temp, |t| format!("{t:.0}°C")), opt(s.gpu_clock_mhz, |c| format!("{c:.0} MHz"))].into_iter().flatten().collect();
    if !gpu.is_empty() {
        let title = if s.gpu_name.is_empty() { "GPU".to_string() } else { s.gpu_name.clone() };
        let col = if s.gpu_limit.is_empty() { TEXT } else if s.gpu_limit.contains("hot") { BAD } else { OK };
        let mut v = gpu.join(" · ");
        if !s.gpu_limit.is_empty() {
            v = format!("{} · {v}", s.gpu_limit);
        }
        row(ui, None, &title, DIM, None, Trail::Value(v, col));
        if let (Some(pw), Some(pl)) = (s.gpu_power_w, s.gpu_power_limit_w) {
            let vram = match (s.vram_used_gb, s.vram_total_gb) {
                (Some(u), Some(t)) => format!("VRAM {u:.1}/{t:.0} GB · "),
                _ => String::new(),
            };
            row(ui, None, "Power & memory", DIM, None, Trail::Value(format!("{vram}{pw:.0}/{pl:.0} W"), TEXT));
        }
    }
    if s.ram_total_gb > 0.0 {
        row(ui, None, "CPU", DIM, None, Trail::Value(format!("{:.0}% · busiest core {:.0}%", s.cpu, s.cpu_core_max), TEXT));
        row(ui, None, "RAM", DIM, None, Trail::Value(format!("{:.1}/{:.0} GB", s.ram_used_gb, s.ram_total_gb), TEXT));
    }
}

/// The PC's on-screen display: small, click-through, never takes focus.
fn perf_osd(ctx: &egui::Context, v: &PerfView, corner: u8) {
    let screen = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
    let size = vec2(236.0, 104.0);
    let m = 16.0;
    let pos = match corner {
        1 => pos2(screen.x - size.x - m, m),
        2 => pos2(m, screen.y - size.y - m - 48.0),
        3 => pos2(screen.x - size.x - m, screen.y - size.y - m - 48.0),
        _ => pos2(m, m),
    };
    let builder = egui::ViewportBuilder::default()
        .with_title("lagless stats")
        .with_decorations(false)
        .with_always_on_top()
        .with_transparent(true)
        .with_mouse_passthrough(true)
        .with_taskbar(false)
        .with_resizable(false)
        .with_active(false)
        .with_inner_size(size)
        .with_position(pos);
    let s = v.snap.clone();
    let history = v.history.clone();
    ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("lagless-osd"), builder, |ui, _| {
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let rect = ui.max_rect();
            let p = ui.painter();
            p.rect_filled(rect, 14.0, Color32::from_rgba_unmultiplied(16, 16, 18, 200));
            p.rect_stroke(rect.shrink(0.5), 14.0, Stroke::new(1.0, Color32::from_white_alpha(20)), egui::StrokeKind::Inside);
            let g = one_line(p, &format!("{:.0}", s.fps), semi(30.0), TEXT, 120.0);
            let gs = g.size();
            p.galley(pos2(rect.min.x + 12.0, rect.min.y + 6.0), g, TEXT);
            let u = one_line(p, "FPS", semi(11.0), fps_color(s.fps), 40.0);
            p.galley(pos2(rect.min.x + 16.0 + gs.x, rect.min.y + 6.0 + gs.y - u.size().y - 6.0), u, TEXT);
            let lows = format!("1% {:.0}  ·  {:.1} ms", s.low1, s.frame_ms);
            let lg = one_line(p, &lows, reg(11.5), DIM, 110.0);
            p.galley(pos2(rect.max.x - 12.0 - lg.size().x, rect.min.y + 16.0), lg, DIM);
            let graph = Rect::from_min_max(pos2(rect.min.x + 8.0, rect.min.y + 46.0), pos2(rect.max.x - 8.0, rect.min.y + 78.0));
            paint_graph(p, graph, &history, false);
            let mut parts = Vec::new();
            if let Some(u) = s.gpu_util {
                parts.push(format!("GPU {u:.0}%{}", s.gpu_temp.map_or(String::new(), |t| format!(" {t:.0}°"))));
            }
            parts.push(format!("CPU {:.0}%", s.cpu));
            let bg = one_line(p, &parts.join("  ·  "), reg(11.5), TEXT, size.x - 24.0);
            p.galley(pos2(rect.min.x + 12.0, rect.max.y - 21.0), bg, TEXT);
        });
    });
}
