//! lagless — stream your PC's audio to a Mac with as little latency as the link allows.
//!
//! No arguments: the desktop app. With a subcommand: headless command-line mode.

// Release builds on Windows are GUI apps (no console window flashing up).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod codec;
mod engine;
mod gui;
mod net;
mod playout;
mod protocol;
mod receiver;
mod sender;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = "Stream audio PC -> Mac over Wi-Fi, Ethernet, USB or Bluetooth.\n\nRun with no arguments: Windows sends, macOS receives.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Capture this computer's audio and stream it (run on the PC)
    Send {
        /// Receiver address (skips auto-discovery), e.g. 192.168.1.20 or 169.254.10.2:47801
        #[arg(long)]
        to: Option<String>,
        /// Receiver port (47801 = speakers, 47802 = mic link)
        #[arg(long, default_value_t = protocol::AUDIO_PORT)]
        port: u16,
        /// Pick a receiver by (part of) its name when several are running
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        codec: sender::CodecChoice,
        /// Frame length in ms. Smaller = lower latency, more packets per second
        #[arg(long, default_value_t = 1.0)]
        frame_ms: f64,
        /// Redundant copies of earlier frames in each packet (repairs packet loss with no added delay)
        #[arg(long, default_value_t = 1)]
        fec: usize,
        /// Opus bitrate in kbps (only used when Opus is active)
        #[arg(long, default_value_t = 320)]
        bitrate: u32,
        #[arg(long, value_enum, default_value = "loopback")]
        source: sender::Source,
        /// Capture device (part of its name); default = system default output
        #[arg(long)]
        device: Option<String>,
        #[arg(long, default_value_t = 0.0, hide = true)]
        sim_loss: f64,
        /// Sample rate for --source tone
        #[arg(long, default_value_t = 48000, hide = true)]
        tone_rate: u32,
    },
    /// Play an incoming stream (run on the Mac)
    Receive {
        #[arg(long, default_value_t = protocol::AUDIO_PORT)]
        port: u16,
        /// Output device (part of its name), e.g. your DAC/interface; default = system output
        #[arg(long)]
        device: Option<String>,
        /// Lowest jitter buffer to aim for, in ms (it adapts upward only if the link needs it)
        #[arg(long, default_value_t = 2.0)]
        min_latency: f64,
        /// Never let the jitter buffer grow beyond this, in ms
        #[arg(long, default_value_t = 120.0)]
        max_latency: f64,
        /// Audio device buffer in frames (64 ≈ 1.3 ms at 48k). 0 = system default
        #[arg(long, default_value_t = 64)]
        buffer: u32,
        /// Name shown to senders (default: this computer's name)
        #[arg(long)]
        name: Option<String>,
        #[arg(long, hide = true)]
        null_output: bool,
    },
    /// List audio devices
    Devices,
    /// Install the latest release now
    Update,
}

fn main() {
    if std::env::args_os().len() <= 1 {
        if let Err(e) = gui::run() {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    attach_console();
    if let Err(e) = run() {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
}

/// A windows_subsystem="windows" exe has no console; borrow the terminal it was started from.
#[cfg(windows)]
fn attach_console() {
    extern "system" {
        fn AttachConsole(pid: u32) -> i32;
    }
    unsafe {
        AttachConsole(u32::MAX);
    }
}

fn run() -> Result<()> {
    let cmd = Cli::parse().cmd.expect("subcommand");
    let sh = engine::Shared::new();
    let worker = sh.clone();
    let job: Box<dyn FnOnce() -> Result<()> + Send> = match cmd {
        Cmd::Send { to, port, name, codec, frame_ms, fec, bitrate, source, device, sim_loss, tone_rate } => {
            let o = sender::Options { to, port, name, codec, frame_ms, redundancy: fec.min(8), bitrate_kbps: bitrate, source, device, sim_loss, tone_rate };
            Box::new(move || sender::run(o, worker))
        }
        Cmd::Receive { port, device, min_latency, max_latency, buffer, name, null_output } => {
            let o = receiver::Options { port, device, min_latency_ms: min_latency, max_latency_ms: max_latency, buffer_frames: buffer, name, null_output };
            Box::new(move || receiver::run(o, worker))
        }
        Cmd::Devices => return audio::list_devices(),
        Cmd::Update => {
            match update::Updater::update_blocking()? {
                Some(v) => println!("Updated to {v}. Restart lagless to use it."),
                None => println!("lagless {} is the latest version.", update::VERSION),
            }
            return Ok(());
        }
    };
    let handle = std::thread::spawn(job);
    while !handle.is_finished() {
        std::thread::sleep(std::time::Duration::from_millis(250));
        for e in sh.take_events() {
            println!("\n{e}");
        }
        print_status(&sh.snapshot());
    }
    handle.join().unwrap()
}

fn print_status(s: &engine::Status) {
    use std::io::Write;
    let line = match &s.phase {
        engine::Phase::Starting | engine::Phase::Searching => "searching…".to_string(),
        engine::Phase::Error(e) => format!("error: {e}"),
        phase => format!(
            "{} {} via {} | {} {} kHz | rtt {:.2} ms | loss {:.1}% | buffer {:.1}/{:.1} ms | fixed {} lost {} rebuffers {} | ≈{:.0} ms",
            if *phase == engine::Phase::Streaming { "▶" } else { "‖" },
            s.peer_name,
            s.link_name,
            s.codec,
            s.sample_rate as f32 / 1000.0,
            s.rtt_ms,
            s.loss_pct,
            s.buffer_ms,
            s.target_ms,
            s.fixed,
            s.lost,
            s.underruns,
            s.latency_ms
        ),
    };
    print!("\r{line}   ");
    let _ = std::io::stdout().flush();
}
