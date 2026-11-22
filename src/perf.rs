//! Game performance stats for the PC: FPS and frame times, GPU and CPU load, and a plain-language
//! verdict on what's holding the frame rate back. Shown on the Mac (sent over the lagless link)
//! and/or in a small click-through overlay on the PC.
//!
//! Where the numbers come from, without touching the game (no injection, nothing for anti-cheat
//! to object to):
//! * Frame times: Windows' event tracing. Every DXGI Present (one per frame, D3D11 and D3D12)
//!   is logged by Windows with a high-resolution timestamp; that's what PresentMon reads too.
//!   Starting such a trace needs admin or the "Performance Log Users" group (see
//!   `allow_fps_counter`).
//! * GPU: NVIDIA's NVML (nvml.dll ships with the driver): load, temperature, clock, power,
//!   VRAM, and why the GPU is slowing itself down (power limit, heat).
//! * CPU/RAM and "what else is running": sysinfo.
//!
//! `LAGLESS_FAKE_PERF=1` produces a synthetic game (for testing the UI on any OS).

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Width of one frame-time graph point (the worst frame in each slice is kept, so spikes show).
pub const GRAPH_SLICE_MS: f64 = 50.0;

const GIB: f32 = 1_073_741_824.0;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PerfSnapshot {
    /// The game being measured (the focused program that's drawing frames), "" when none.
    pub game: String,
    pub fps: f32,
    /// FPS equivalent of the 99th / 99.9th percentile frame time over the last 10 s.
    pub low1: f32,
    pub low01: f32,
    pub frame_ms: f32,
    /// New frame-time graph points (ms) since the last snapshot, one per GRAPH_SLICE_MS.
    pub graph: Vec<f32>,
    pub gpu_name: String,
    pub gpu_util: Option<f32>,
    pub gpu_temp: Option<f32>,
    pub gpu_clock_mhz: Option<f32>,
    pub gpu_power_w: Option<f32>,
    pub gpu_power_limit_w: Option<f32>,
    pub vram_used_gb: Option<f32>,
    pub vram_total_gb: Option<f32>,
    /// Why the GPU is slowing itself down right now ("" = it isn't).
    pub gpu_limit: String,
    pub cpu: f32,
    /// Busiest single core: games are usually limited by one thread, which the average hides.
    pub cpu_core_max: f32,
    pub ram_used_gb: f32,
    pub ram_total_gb: f32,
    /// "GPU-limited", "CPU-limited", "Capped at 165"… and what to do about it.
    pub verdict: String,
    pub tip: String,
    pub stutters: u32,
    /// The latest stutter and its likely cause.
    pub last_stutter: String,
    /// Why there's no FPS ("" = fine): e.g. permission needed.
    pub fps_problem: String,
}

impl PerfSnapshot {
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_json(b: &[u8]) -> Option<Self> {
        serde_json::from_slice(b).ok()
    }
}

/// What the overlay/Mac show when the FPS counter can't start because of permissions.
pub const NEEDS_PERMISSION: &str = "needs permission";

#[derive(Default)]
struct Frames {
    /// Per process: (timestamp ms, frame time ms), last ~10 s.
    by_pid: HashMap<u32, VecDeque<(f64, f32)>>,
    last_ts: HashMap<u32, f64>,
    problem: String,
}

impl Frames {
    fn present(&mut self, pid: u32, t_ms: f64) {
        if let Some(prev) = self.last_ts.insert(pid, t_ms) {
            let dt = (t_ms - prev) as f32;
            if dt > 0.0 && dt < 2000.0 {
                let q = self.by_pid.entry(pid).or_default();
                q.push_back((t_ms, dt));
                while q.front().is_some_and(|(t, _)| t_ms - t > 10_000.0) {
                    q.pop_front();
                }
            }
        }
    }
}

pub struct Monitor {
    state: Arc<Mutex<PerfSnapshot>>,
    fresh: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Monitor {
    pub fn start(wake: impl Fn() + Send + 'static) -> Monitor {
        let state = Arc::new(Mutex::new(PerfSnapshot::default()));
        let fresh = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, fr, sp) = (state.clone(), fresh.clone(), stop.clone());
        let handle = std::thread::Builder::new()
            .name("perf".into())
            .spawn(move || {
                run(st, sp, move || {
                    fr.store(true, Relaxed);
                    wake()
                })
            })
            .ok();
        Monitor { state, fresh, stop, handle }
    }

    /// New numbers since the last call, if any. Graph points are handed over once (the caller
    /// keeps the history).
    pub fn take_fresh(&self) -> Option<PerfSnapshot> {
        if !self.fresh.swap(false, Relaxed) {
            return None;
        }
        let mut s = self.state.lock().unwrap();
        let out = s.clone();
        s.graph.clear();
        Some(out)
    }
}

fn run(state: Arc<Mutex<PerfSnapshot>>, stop: Arc<AtomicBool>, wake: impl Fn()) {
    let fake = std::env::var_os("LAGLESS_FAKE_PERF").is_some();
    let frames = Arc::new(Mutex::new(Frames::default()));
    let _source = if fake { None } else { frame_source::start(frames.clone()) };
    let mut gpu = if fake { None } else { gpu::Gpu::open() };
    let mut sys = sysinfo::System::new().ok();
    let mut fake_game = fake.then(FakeGame::new);

    let my_pid = std::process::id();
    let mut cursor_ms = 0.0f64; // graph: frames up to here have been turned into points
    let mut stutters = 0u32;
    let mut last_stutter = String::new();
    let mut last_hw = Instant::now() - Duration::from_secs(10);
    let mut last_procs = Instant::now() - Duration::from_secs(10);
    let mut hw = Hardware::default();
    let mut game_pid: Option<u32> = None;
    let t0 = Instant::now();

    while !stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        let now_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Hardware: once a second (processes every 2 s; listing them costs a few ms).
        if last_hw.elapsed() >= Duration::from_secs(1) {
            last_hw = Instant::now();
            if let Some(s) = sys.as_mut() {
                s.refresh_cpu_usage();
                s.refresh_memory();
                if last_procs.elapsed() >= Duration::from_secs(2) {
                    last_procs = Instant::now();
                    s.refresh_processes_specifics(sysinfo::ProcessesToUpdate::All, true, sysinfo::ProcessRefreshKind::nothing().with_cpu());
                }
                hw.cpu = s.global_cpu_usage();
                hw.cpu_core_max = s.cpus().iter().map(|c| c.usage()).fold(0.0, f32::max);
                hw.cores = s.cpus().len().max(1);
                hw.ram_used_gb = s.used_memory() as f32 / GIB;
                hw.ram_total_gb = s.total_memory() as f32 / GIB;
                // Busiest program other than the game and lagless (a stutter suspect).
                hw.top = s
                    .processes()
                    .iter()
                    .filter(|(pid, _)| Some(pid.as_u32()) != game_pid && pid.as_u32() != my_pid && pid.as_u32() != 0)
                    .map(|(_, p)| (p.name().to_string_lossy().into_owned(), p.cpu_usage() / hw.cores as f32))
                    .filter(|(n, _)| !n.eq_ignore_ascii_case("System Idle Process") && !n.eq_ignore_ascii_case("Idle"))
                    .max_by(|a, b| a.1.total_cmp(&b.1));
            }
            hw.gpu = gpu.as_mut().map(|g| g.read());
            if let Some(f) = fake_game.as_mut() {
                hw.gpu = Some(f.gpu());
                hw.top = Some(("Discord.exe".into(), 3.0));
            }
        }

        // Frames of the game: the focused program, if it draws; else whoever draws the most.
        if let Some(f) = fake_game.as_mut() {
            let mut fr = frames.lock().unwrap();
            for t in f.frames_until(now_ms) {
                fr.present(4242, t);
            }
        }
        let mut fr = frames.lock().unwrap();
        let fg = if fake { Some(4242) } else { foreground_pid() };
        let drawing = |pid: &u32, q: &VecDeque<(f64, f32)>| *pid != my_pid && q.back().is_some_and(|(t, _)| *t > latest_ts(&fr.by_pid) - 2000.0);
        game_pid = fg.filter(|p| fr.by_pid.get(p).is_some_and(|q| drawing(p, q))).or_else(|| {
            fr.by_pid.iter().filter(|(p, q)| drawing(p, q) && !is_system_drawer(*p, sys.as_ref())).max_by_key(|(_, q)| q.len()).map(|(p, _)| *p)
        });
        // Forget programs that stopped drawing.
        let newest = latest_ts(&fr.by_pid);
        fr.by_pid.retain(|_, q| q.back().is_some_and(|(t, _)| newest - t < 15_000.0));

        let mut snap = PerfSnapshot { fps_problem: fr.problem.clone(), ..Default::default() };
        if fake {
            snap.fps_problem.clear();
        } else if !cfg!(windows) && snap.fps_problem.is_empty() {
            snap.fps_problem = "FPS needs Windows".into();
        }
        let mut graph = Vec::new();
        if let Some(q) = game_pid.and_then(|p| fr.by_pid.get(&p)) {
            snap.game = game_pid.map(|p| if fake { "Fortnite".to_string() } else { game_name(p, sys.as_ref()) }).unwrap_or_default();
            let end = q.back().map_or(0.0, |(t, _)| *t);
            let last_sec: Vec<f32> = q.iter().filter(|(t, _)| end - t <= 1000.0).map(|(_, d)| *d).collect();
            if !last_sec.is_empty() {
                let avg = last_sec.iter().sum::<f32>() / last_sec.len() as f32;
                snap.frame_ms = avg;
                snap.fps = 1000.0 / avg;
            }
            let mut all: Vec<f32> = q.iter().map(|(_, d)| *d).collect();
            all.sort_by(f32::total_cmp);
            let pct = |p: f64| all[((all.len() as f64 - 1.0) * p).round() as usize];
            if all.len() >= 20 {
                snap.low1 = 1000.0 / pct(0.99);
                snap.low01 = 1000.0 / pct(0.999);
            }
            // Graph points for slices that are complete (events arrive in ~1 s batches).
            if cursor_ms == 0.0 || cursor_ms < end - 3000.0 {
                cursor_ms = end - 1000.0;
            }
            let recent: Vec<f32> = q.iter().filter(|(t, _)| end - t <= 2000.0).map(|(_, d)| *d).collect();
            let median = {
                let mut r = recent.clone();
                r.sort_by(f32::total_cmp);
                r.get(r.len() / 2).copied().unwrap_or(snap.frame_ms)
            };
            while cursor_ms + GRAPH_SLICE_MS <= end {
                let (a, b) = (cursor_ms, cursor_ms + GRAPH_SLICE_MS);
                let worst = q.iter().filter(|(t, _)| *t > a && *t <= b).map(|(_, d)| *d).fold(0.0f32, f32::max);
                graph.push(if worst > 0.0 { worst } else { graph.last().copied().unwrap_or(median) });
                if worst > (median * 2.5).max(25.0) {
                    stutters += 1;
                    last_stutter = format!("{worst:.0} ms hitch · {}", stutter_cause(&hw));
                }
                cursor_ms = b;
            }
            // What's limiting the frame rate.
            let cv = if recent.len() > 10 {
                let m = recent.iter().sum::<f32>() / recent.len() as f32;
                (recent.iter().map(|d| (d - m).powi(2)).sum::<f32>() / recent.len() as f32).sqrt() / m
            } else {
                1.0
            };
            let util = hw.gpu.as_ref().and_then(|g| g.util);
            (snap.verdict, snap.tip) = match util {
                Some(u) if u >= 95.0 => ("GPU-limited".into(), "Lower resolution or graphics settings for more FPS".into()),
                u if cv < 0.06 && u.is_none_or(|u| u < 92.0) => (format!("Capped at {:.0}", snap.fps), "Running at your frame cap or V-Sync".into()),
                Some(_) => ("CPU-limited".into(), "Graphics settings won't help much; close background apps".into()),
                None => (String::new(), String::new()),
            };
        } else {
            cursor_ms = 0.0;
        }
        drop(fr);

        snap.cpu = hw.cpu;
        snap.cpu_core_max = hw.cpu_core_max;
        snap.ram_used_gb = hw.ram_used_gb;
        snap.ram_total_gb = hw.ram_total_gb;
        if let Some(g) = &hw.gpu {
            snap.gpu_name = g.name.clone();
            snap.gpu_util = g.util;
            snap.gpu_temp = g.temp;
            snap.gpu_clock_mhz = g.clock_mhz;
            snap.gpu_power_w = g.power_w;
            snap.gpu_power_limit_w = g.power_limit_w;
            snap.vram_used_gb = g.vram_used_gb;
            snap.vram_total_gb = g.vram_total_gb;
            snap.gpu_limit = g.limit.clone();
        }
        snap.stutters = stutters;
        snap.last_stutter = last_stutter.clone();
        {
            let mut s = state.lock().unwrap();
            let mut pending = std::mem::take(&mut s.graph);
            pending.extend(graph);
            // Nobody's reading (e.g. window hidden): don't grow forever.
            let n = pending.len();
            if n > 400 {
                pending.drain(..n - 400);
            }
            snap.graph = pending;
            *s = snap;
        }
        wake();
    }
}

fn latest_ts(m: &HashMap<u32, VecDeque<(f64, f32)>>) -> f64 {
    m.values().filter_map(|q| q.back().map(|(t, _)| *t)).fold(0.0, f64::max)
}

#[derive(Default)]
struct Hardware {
    cpu: f32,
    cpu_core_max: f32,
    cores: usize,
    ram_used_gb: f32,
    ram_total_gb: f32,
    top: Option<(String, f32)>,
    gpu: Option<GpuReading>,
}

#[derive(Clone, Debug, Default)]
struct GpuReading {
    name: String,
    util: Option<f32>,
    temp: Option<f32>,
    clock_mhz: Option<f32>,
    power_w: Option<f32>,
    power_limit_w: Option<f32>,
    vram_used_gb: Option<f32>,
    vram_total_gb: Option<f32>,
    limit: String,
}

/// The likeliest reason for a hitch, from what the PC was doing at the time.
fn stutter_cause(hw: &Hardware) -> String {
    let g = hw.gpu.clone().unwrap_or_default();
    if g.limit.contains("hot") {
        return "GPU was too hot and slowed down".into();
    }
    if let (Some(u), Some(t)) = (g.vram_used_gb, g.vram_total_gb) {
        if u / t > 0.97 {
            return "VRAM full, textures swapping".into();
        }
    }
    if let Some((name, share)) = &hw.top {
        if *share >= 15.0 {
            return format!("{} was using {share:.0}% CPU", pretty_name(name));
        }
    }
    if hw.cpu_core_max >= 97.0 {
        return "a CPU core was maxed out".into();
    }
    "inside the game (loading or shader compile)".into()
}

/// Programs that draw but aren't games.
fn is_system_drawer(pid: &u32, sys: Option<&sysinfo::System>) -> bool {
    let name = sys.and_then(|s| s.process(sysinfo::Pid::from_u32(*pid))).map(|p| p.name().to_string_lossy().to_lowercase()).unwrap_or_default();
    ["dwm", "explorer", "searchhost", "startmenuexperiencehost", "textinputhost", "shellexperiencehost", "msedgewebview2", "lagless"].iter().any(|n| name.starts_with(n))
}

fn game_name(pid: u32, sys: Option<&sysinfo::System>) -> String {
    let raw = sys.and_then(|s| s.process(sysinfo::Pid::from_u32(pid))).map(|p| p.name().to_string_lossy().into_owned()).unwrap_or_else(|| format!("PID {pid}"));
    pretty_name(&raw)
}

/// "FortniteClient-Win64-Shipping.exe" -> "Fortnite", "Discord.exe" -> "Discord".
pub fn pretty_name(raw: &str) -> String {
    let mut n = raw.trim_end_matches(".exe").trim_end_matches(".EXE").to_string();
    for suffix in ["-Win64-Shipping", "-Win32-Shipping", "_x64", "-Win64", "Client"] {
        if let Some(stripped) = n.strip_suffix(suffix) {
            n = stripped.to_string();
        }
    }
    if n.is_empty() {
        raw.to_string()
    } else {
        n
    }
}

fn foreground_pid() -> Option<u32> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let w = GetForegroundWindow();
        let mut pid = 0u32;
        GetWindowThreadProcessId(w, Some(&mut pid));
        return (pid != 0).then_some(pid);
    }
    #[allow(unreachable_code)]
    None
}

// ---------------------------------------------------------------- frame source (Windows ETW)

#[cfg(windows)]
mod frame_source {
    use super::Frames;
    use ferrisetw::provider::Provider;
    use ferrisetw::trace::{stop_trace_by_name, TraceProperties, TraceTrait, UserTrace};
    use std::sync::{Arc, Mutex};

    /// Microsoft-Windows-DXGI; event 42 = Present start (one per frame, any DXGI game).
    const DXGI: &str = "CA11C036-0102-4A2D-A6AD-F03CFED5D3C9";
    const PRESENT_START: u16 = 42;
    const SESSION: &str = "lagless-fps";

    pub struct Source(#[allow(dead_code)] UserTrace);

    pub fn start(frames: Arc<Mutex<Frames>>) -> Option<Source> {
        let mut freq = 0i64;
        unsafe {
            let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut freq);
        }
        let ticks_per_ms = (freq.max(1) as f64) / 1000.0;
        let fr = frames.clone();
        let build = || {
            let fr = fr.clone();
            Provider::by_guid(DXGI)
                .add_callback(move |rec: &ferrisetw::EventRecord, _: &ferrisetw::schema_locator::SchemaLocator| {
                    if rec.event_id() == PRESENT_START {
                        // The session runs on the QPC clock (ferrisetw's default).
                        fr.lock().unwrap().present(rec.process_id(), rec.raw_timestamp() as f64 / ticks_per_ms);
                    }
                })
                .build()
        };
        let props = TraceProperties { flush_timer: std::time::Duration::from_secs(1), ..Default::default() };
        let attempt = || UserTrace::new().named(SESSION.to_string()).set_trace_properties(props).enable(build()).start_and_process();
        let started = match attempt() {
            // Left over from a lagless that didn't exit cleanly: take it over.
            Err(ferrisetw::trace::TraceError::EtwNativeError(ferrisetw::native::EvntraceNativeError::AlreadyExist)) => {
                let _ = stop_trace_by_name(SESSION);
                attempt()
            }
            r => r,
        };
        match started {
            Ok(t) => {
                let _ = t.events_handled();
                Some(Source(t))
            }
            Err(e) => {
                let denied = matches!(&e, ferrisetw::trace::TraceError::EtwNativeError(ferrisetw::native::EvntraceNativeError::IoError(io)) if io.raw_os_error() == Some(5));
                frames.lock().unwrap().problem = if denied { super::NEEDS_PERMISSION.into() } else { format!("FPS counter failed: {e:?}") };
                None
            }
        }
    }
}

#[cfg(not(windows))]
mod frame_source {
    pub struct Source;
    pub fn start(_frames: std::sync::Arc<std::sync::Mutex<super::Frames>>) -> Option<Source> {
        None
    }
}

/// Windows: let this user start the FPS trace without running lagless as admin, by adding them
/// to the built-in "Performance Log Users" group (SID S-1-5-32-559, so it works in any
/// language). One admin prompt; takes effect after signing out and back in.
#[cfg(windows)]
pub fn allow_fps_counter() -> anyhow::Result<()> {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(d), Ok(u)) => format!("{d}\\{u}"),
        (_, Ok(u)) => u,
        _ => anyhow::bail!("couldn't tell who's signed in"),
    };
    let user = user.replace('\'', "''");
    let args = format!("-NoProfile -WindowStyle Hidden -Command \"Add-LocalGroupMember -SID 'S-1-5-32-559' -Member '{user}' -ErrorAction SilentlyContinue\"");
    let r = unsafe { ShellExecuteW(None, w!("runas"), w!("powershell.exe"), &HSTRING::from(args), None, SW_HIDE) };
    if r.0 as isize <= 32 {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

// ---------------------------------------------------------------- GPU (NVIDIA NVML)

#[cfg(windows)]
mod gpu {
    use super::{GpuReading, GIB};
    use nvml_wrapper::bitmasks::device::ThrottleReasons;
    use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
    use nvml_wrapper::Nvml;

    pub struct Gpu {
        nvml: Nvml,
        name: String,
    }

    impl Gpu {
        pub fn open() -> Option<Gpu> {
            let nvml = Nvml::init().ok()?;
            let name = nvml.device_by_index(0).ok()?.name().unwrap_or_default();
            Some(Gpu { nvml, name: name.replace("NVIDIA ", "").replace("GeForce ", "") })
        }

        pub fn read(&mut self) -> GpuReading {
            let Ok(d) = self.nvml.device_by_index(0) else { return GpuReading::default() };
            let mem = d.memory_info().ok();
            let reasons = d.current_throttle_reasons().unwrap_or(ThrottleReasons::empty());
            let limit = if reasons.intersects(ThrottleReasons::HW_THERMAL_SLOWDOWN | ThrottleReasons::SW_THERMAL_SLOWDOWN) {
                "Too hot"
            } else if reasons.intersects(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN) {
                "Power limit"
            } else {
                ""
            };
            GpuReading {
                name: self.name.clone(),
                util: d.utilization_rates().ok().map(|u| u.gpu as f32),
                temp: d.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32),
                clock_mhz: d.clock_info(Clock::Graphics).ok().map(|c| c as f32),
                power_w: d.power_usage().ok().map(|p| p as f32 / 1000.0),
                power_limit_w: d.enforced_power_limit().ok().map(|p| p as f32 / 1000.0),
                vram_used_gb: mem.as_ref().map(|m| m.used as f32 / GIB),
                vram_total_gb: mem.as_ref().map(|m| m.total as f32 / GIB),
                limit: limit.into(),
            }
        }
    }
}

#[cfg(not(windows))]
mod gpu {
    pub struct Gpu;
    impl Gpu {
        pub fn open() -> Option<Gpu> {
            None
        }
        pub fn read(&mut self) -> super::GpuReading {
            super::GpuReading::default()
        }
    }
}

// ---------------------------------------------------------------- fake game (testing)

struct FakeGame {
    next_ms: f64,
    n: u64,
}

impl FakeGame {
    fn new() -> Self {
        FakeGame { next_ms: 0.0, n: 0 }
    }

    /// ~160 fps with jitter and a hitch every ~7 s.
    fn frames_until(&mut self, now_ms: f64) -> Vec<f64> {
        let mut out = Vec::new();
        // Like ETW: frames show up about a second late.
        while self.next_ms < now_ms - 1000.0 {
            self.n += 1;
            let jitter = ((self.n as f64 * 12.9898).sin() * 43758.5453).fract().abs() * 1.6;
            let dt = if self.n % 1100 == 0 { 48.0 } else { 5.5 + jitter };
            self.next_ms += dt;
            out.push(self.next_ms);
        }
        out
    }

    fn gpu(&self) -> GpuReading {
        let w = (self.n as f64 / 300.0).sin() as f32;
        GpuReading {
            name: "RTX 2080".into(),
            util: Some(88.0 + 8.0 * w),
            temp: Some(68.0 + 3.0 * w),
            clock_mhz: Some(1890.0 + 30.0 * w),
            power_w: Some(196.0 + 15.0 * w),
            power_limit_w: Some(225.0),
            vram_used_gb: Some(5.4),
            vram_total_gb: Some(8.0),
            limit: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(pretty_name("FortniteClient-Win64-Shipping.exe"), "Fortnite");
        assert_eq!(pretty_name("Discord.exe"), "Discord");
        assert_eq!(pretty_name("cs2.exe"), "cs2");
    }

    #[test]
    fn snapshot_json_roundtrip() {
        let s = PerfSnapshot { game: "Fortnite".into(), fps: 240.0, graph: vec![4.1, 4.3], gpu_temp: Some(70.0), ..Default::default() };
        assert_eq!(PerfSnapshot::from_json(&s.to_json()), Some(s));
    }

    #[test]
    fn frame_times_per_process() {
        let mut f = Frames::default();
        for i in 0..10 {
            f.present(7, i as f64 * 4.0);
        }
        let q = &f.by_pid[&7];
        assert_eq!(q.len(), 9);
        assert!(q.iter().all(|(_, d)| (*d - 4.0).abs() < 1e-3));
    }
}
