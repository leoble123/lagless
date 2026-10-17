//! Shared state between the streaming engine (background thread) and the GUI / CLI.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Starting,
    /// Sender: looking for a receiver. Receiver: waiting for a sender.
    Searching,
    /// Linked, but no audio is flowing (nothing playing on the PC).
    Idle,
    Streaming,
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Cable,
    WiFi,
    Thunderbolt,
    Bluetooth,
    Loopback,
    Network,
}

impl LinkKind {
    pub fn label(self) -> &'static str {
        match self {
            LinkKind::Cable => "Cable",
            LinkKind::WiFi => "Wi-Fi",
            LinkKind::Thunderbolt => "Thunderbolt",
            LinkKind::Bluetooth => "Bluetooth",
            LinkKind::Loopback => "This computer",
            LinkKind::Network => "Network",
        }
    }

    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> LinkKind {
        [LinkKind::Cable, LinkKind::WiFi, LinkKind::Thunderbolt, LinkKind::Bluetooth, LinkKind::Loopback]
            .get(v as usize)
            .copied()
            .unwrap_or(LinkKind::Network)
    }

    /// Guess the physical link from the local interface name and remote address.
    pub fn classify(iface: &str, remote: IpAddr) -> LinkKind {
        let n = iface.to_lowercase();
        if remote.is_loopback() {
            LinkKind::Loopback
        } else if n.contains("bluetooth") || n.contains("bnep") || n.contains("pan") {
            LinkKind::Bluetooth
        } else if n.contains("wi-fi") || n.contains("wifi") || n.contains("wlan") || n.contains("wireless") || n.contains("airport") {
            LinkKind::WiFi
        } else if n.contains("thunderbolt") || n.starts_with("bridge") {
            LinkKind::Thunderbolt
        } else if n.contains("ethernet") || n.contains("lan") || n.contains("usb") || n.starts_with("eth") || n.starts_with("en") {
            LinkKind::Cable
        } else if matches!(remote, IpAddr::V4(v) if v.is_link_local()) {
            LinkKind::Cable
        } else {
            LinkKind::Network
        }
    }
}

#[derive(Clone, Debug)]
pub struct Status {
    pub phase: Phase,
    pub peer_name: String,
    pub peer_addr: String,
    pub link: LinkKind,
    pub link_name: String,
    pub device_name: String,
    pub codec: String,
    pub sample_rate: u32,
    pub lossless: bool,
    pub rtt_ms: f32,
    pub loss_pct: f32,
    pub buffer_ms: f32,
    pub target_ms: f32,
    pub device_ms: f32,
    /// Best estimate of PC-plays-it -> sound-leaves-the-Mac, in ms (0 = unknown).
    pub latency_ms: f32,
    pub fixed: u32,
    pub lost: u32,
    pub underruns: u32,
    pub fec: usize,
}

impl Default for Status {
    fn default() -> Self {
        Status {
            phase: Phase::Starting,
            peer_name: String::new(),
            peer_addr: String::new(),
            link: LinkKind::Network,
            link_name: String::new(),
            device_name: String::new(),
            codec: String::new(),
            sample_rate: 0,
            lossless: true,
            rtt_ms: 0.0,
            loss_pct: 0.0,
            buffer_ms: 0.0,
            target_ms: 0.0,
            device_ms: 0.0,
            latency_ms: 0.0,
            fixed: 0,
            lost: 0,
            underruns: 0,
            fec: 0,
        }
    }
}

#[derive(Default)]
pub struct Shared {
    pub stop: AtomicBool,
    pub status: Mutex<Status>,
    /// Peak levels since the UI last looked (max-accumulated by the engine, taken by the UI).
    levels: Mutex<[f32; 2]>,
    events: Mutex<VecDeque<String>>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Shared::default())
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Relaxed)
    }

    pub fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    pub fn snapshot(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn event(&self, msg: impl Into<String>) {
        let mut e = self.events.lock().unwrap();
        e.push_back(msg.into());
        if e.len() > 50 {
            e.pop_front();
        }
    }

    pub fn take_events(&self) -> Vec<String> {
        self.events.lock().unwrap().drain(..).collect()
    }

    /// Feed interleaved samples; tracks per-channel peaks (mono feeds both).
    pub fn meter(&self, samples: &[f32], channels: usize) {
        let mut pk = [0f32; 2];
        for f in samples.chunks_exact(channels.max(1)) {
            pk[0] = pk[0].max(f[0].abs());
            pk[1] = pk[1].max(f.get(1).unwrap_or(&f[0]).abs());
        }
        let mut l = self.levels.lock().unwrap();
        l[0] = l[0].max(pk[0]);
        l[1] = l[1].max(pk[1]);
    }

    pub fn take_levels(&self) -> [f32; 2] {
        std::mem::take(&mut *self.levels.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_links() {
        let lan: IpAddr = "192.168.1.4".parse().unwrap();
        let ll: IpAddr = "169.254.3.3".parse().unwrap();
        assert_eq!(LinkKind::classify("Wi-Fi", lan), LinkKind::WiFi);
        assert_eq!(LinkKind::classify("Ethernet 2", lan), LinkKind::Cable);
        assert_eq!(LinkKind::classify("Bluetooth Network Connection", lan), LinkKind::Bluetooth);
        assert_eq!(LinkKind::classify("Thunderbolt Bridge", lan), LinkKind::Thunderbolt);
        assert_eq!(LinkKind::classify("weird0", ll), LinkKind::Cable);
        assert_eq!(LinkKind::classify("x", "127.0.0.1".parse().unwrap()), LinkKind::Loopback);
    }

    #[test]
    fn meters_accumulate_and_reset() {
        let s = Shared::new();
        s.meter(&[0.1, -0.5, 0.3, 0.2], 2);
        s.meter(&[0.2], 1);
        assert_eq!(s.take_levels(), [0.3, 0.5]);
        assert_eq!(s.take_levels(), [0.0, 0.0]);
    }
}
