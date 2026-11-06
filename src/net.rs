//! Sockets, network interfaces and receiver discovery.

use crate::protocol::{self, Beacon, DISCOVERY_PORT};
use anyhow::Result;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub fn udp_socket(port: u16, broadcast: bool, reuse: bool) -> Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if reuse {
        s.set_reuse_address(true)?;
    }
    // DSCP EF (46): Wi-Fi maps this to the WMM "voice" queue, which jumps ahead of bulk traffic.
    let _ = s.set_tos(0xB8);
    let _ = s.set_recv_buffer_size(1 << 20);
    let _ = s.set_send_buffer_size(1 << 20);
    if broadcast {
        s.set_broadcast(true)?;
    }
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
    Ok(s.into())
}

struct V4If {
    name: String,
    ip: Ipv4Addr,
    mask: Ipv4Addr,
}

fn v4_interfaces() -> Vec<V4If> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) => Some(V4If { name: i.name, ip: a.ip, mask: a.netmask }),
            _ => None,
        })
        .collect()
}

/// Human-readable name of the local interface that reaches `remote` (e.g. "Ethernet 2", "Wi-Fi").
pub fn route_name(remote: IpAddr) -> String {
    let IpAddr::V4(r) = remote else { return "?".into() };
    if r.is_loopback() {
        return "loopback".into();
    }
    for i in v4_interfaces() {
        let m = u32::from(i.mask);
        if u32::from(i.ip) & m == u32::from(r) & m {
            return friendly_iface(&i.name);
        }
    }
    "routed".into()
}

/// macOS names interfaces en0/en5/bridge0; map them to "Wi-Fi", "USB 10/100/1000 LAN", etc.
#[cfg(target_os = "macos")]
fn friendly_iface(dev: &str) -> String {
    use std::sync::OnceLock;
    static PORTS: OnceLock<HashMap<String, String>> = OnceLock::new();
    let ports = PORTS.get_or_init(|| {
        let out = std::process::Command::new("networksetup").arg("-listallhardwareports").output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let mut map = HashMap::new();
        let mut port = None;
        for line in text.lines() {
            if let Some(p) = line.strip_prefix("Hardware Port: ") {
                port = Some(p.to_string());
            } else if let (Some(d), Some(p)) = (line.strip_prefix("Device: "), port.take()) {
                map.insert(d.to_string(), p);
            }
        }
        map
    });
    ports.get(dev).cloned().unwrap_or_else(|| dev.to_string())
}

#[cfg(not(target_os = "macos"))]
fn friendly_iface(dev: &str) -> String {
    dev.to_string()
}

/// Receiver side: announce ourselves on every interface, once a second, while `alive` exists.
pub fn spawn_beacon(receiver_id: u64, name: String, audio_port: u16, alive: Weak<()>) -> Result<()> {
    let sock = udp_socket(0, true, false)?;
    let pkt = protocol::write_beacon(&Beacon { receiver_id, audio_port, name });
    std::thread::Builder::new().name("beacon".into()).spawn(move || while alive.upgrade().is_some() {
        for t in broadcast_targets() {
            let _ = sock.send_to(&pkt, (t, DISCOVERY_PORT));
        }
        std::thread::sleep(Duration::from_secs(1));
    })?;
    Ok(())
}

/// Every IPv4 broadcast address worth shouting at, plus loopback for same-machine testing.
/// Directed broadcast reaches link-local (cable/USB) and PAN (Bluetooth) subnets too.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = vec![Ipv4Addr::BROADCAST, Ipv4Addr::LOCALHOST];
    for i in v4_interfaces() {
        targets.push(Ipv4Addr::from(u32::from(i.ip) | !u32::from(i.mask)));
    }
    targets
}

pub struct Seen {
    pub name: String,
    pub first_seen: Instant,
    pub addrs: HashMap<SocketAddr, Instant>,
}

pub type Directory = Arc<Mutex<HashMap<u64, Seen>>>;

/// Sender side: collect beacons. A receiver reachable over several links shows up with several
/// addresses; the sender then races them and uses the fastest. Stops when the Directory is dropped.
pub fn spawn_listener() -> Result<Directory> {
    let dir: Directory = Default::default();
    let sock = udp_socket(DISCOVERY_PORT, true, true)?;
    sock.set_read_timeout(Some(Duration::from_millis(250)))?;
    let weak = Arc::downgrade(&dir);
    std::thread::Builder::new().name("discovery".into()).spawn(move || {
        let mut buf = [0u8; 512];
        let mut last_probe = Instant::now() - Duration::from_secs(10);
        loop {
            // Ask as well as listen: answers to our own broadcast get through firewalls that
            // block unsolicited beacons (Windows treats a direct cable as a Public network).
            if last_probe.elapsed() >= Duration::from_secs(1) {
                last_probe = Instant::now();
                for t in broadcast_targets() {
                    for port in [protocol::AUDIO_PORT, protocol::MIC_PORT] {
                        let _ = sock.send_to(protocol::MAGIC_PROBE, (t, port));
                    }
                }
            }
            let res = sock.recv_from(&mut buf);
            let Some(d) = weak.upgrade() else { return };
            let Ok((n, src)) = res else { continue };
            let Some(b) = protocol::parse_beacon(&buf[..n]) else { continue };
            let addr = SocketAddr::new(src.ip(), b.audio_port);
            let mut d = d.lock().unwrap();
            let e = d.entry(b.receiver_id).or_insert_with(|| Seen {
                name: b.name.clone(),
                first_seen: Instant::now(),
                addrs: HashMap::new(),
            });
            e.addrs.insert(addr, Instant::now());
        }
    })?;
    Ok(dir)
}

/// Windows: replace whatever the first-run firewall prompt created (often "allow on Private,
/// block on Public") with one rule allowing lagless on every network. One UAC prompt.
#[cfg(windows)]
pub fn open_firewall() -> Result<()> {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let exe = std::env::current_exe()?.display().to_string();
    let args = format!(
        "/c netsh advfirewall firewall delete rule name=all program=\"{exe}\" & \
         netsh advfirewall firewall add rule name=\"lagless\" dir=in action=allow program=\"{exe}\" enable=yes profile=any"
    );
    let r = unsafe { ShellExecuteW(None, w!("runas"), w!("cmd.exe"), &HSTRING::from(args), None, SW_HIDE) };
    if r.0 as isize <= 32 {
        anyhow::bail!("Windows didn't run the firewall change (cancelled?)");
    }
    Ok(())
}

/// macOS: AWDL (the AirDrop/Continuity radio) makes Wi-Fi hop channels about once a second,
/// which shows up as regular latency spikes. Turning it off lasts until reboot or until
/// AirDrop is next used. Asks for an admin password.
#[cfg(target_os = "macos")]
pub fn disable_awdl() -> Result<()> {
    let ok = std::process::Command::new("osascript")
        .args(["-e", "do shell script \"/sbin/ifconfig awdl0 down\" with administrator privileges"])
        .status()?
        .success();
    if !ok {
        anyhow::bail!("cancelled");
    }
    Ok(())
}

pub fn hostname() -> String {
    #[cfg(target_os = "macos")]
    if let Ok(o) = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output() {
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "lagless".into())
}

pub fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.write_u32(std::process::id());
    h.finish()
}

/// Windows reports ICMP "port unreachable" from an earlier send as an error on the next recv.
pub fn is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::Interrupted
    )
}
