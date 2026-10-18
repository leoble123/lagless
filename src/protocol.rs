//! Wire format. Everything is little-endian UDP datagrams, each starting with a 4-byte magic.

pub const DISCOVERY_PORT: u16 = 47800;
pub const AUDIO_PORT: u16 = 47801;
/// Reverse direction: the Mac's microphone streamed to the PC.
pub const MIC_PORT: u16 = 47802;
/// Keep datagrams under a typical 1500-byte MTU (minus IP/UDP and VPN/PAN headroom).
pub const MAX_PACKET: usize = 1400;

pub const MAGIC_AUDIO: &[u8; 4] = b"LGLA";
pub const MAGIC_BEACON: &[u8; 4] = b"LGLB";
pub const MAGIC_PING: &[u8; 4] = b"LGLP";
pub const MAGIC_PONG: &[u8; 4] = b"LGLQ";
pub const MAGIC_REPORT: &[u8; 4] = b"LGLR";
pub const MAGIC_INFO: &[u8; 4] = b"LGLI";

/// Frame flag: first frame after the capture source went quiet (not a network glitch).
pub const FLAG_DISCONT: u8 = 1;
/// Frame flag: this is a redundant (FEC) copy of an already-sent frame.
pub const FLAG_REDUNDANT: u8 = 2;

pub const AUDIO_HEADER_LEN: usize = 4 + 4 + 1 + 1 + 4 + 1;
pub const FRAME_HEADER_LEN: usize = 4 + 1 + 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Pcm16 = 1,
    Pcm24 = 2,
    Opus = 3,
}

impl Codec {
    pub fn from_u8(v: u8) -> Option<Codec> {
        match v {
            1 => Some(Codec::Pcm16),
            2 => Some(Codec::Pcm24),
            3 => Some(Codec::Opus),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Pcm16 => "pcm16",
            Codec::Pcm24 => "pcm24",
            Codec::Opus => "opus",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioHeader {
    pub stream_id: u32,
    pub codec: Codec,
    pub channels: u8,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub seq: u32,
    pub flags: u8,
    pub data: &'a [u8],
}

pub fn write_audio_header(buf: &mut Vec<u8>, h: &AudioHeader, nframes: u8) {
    buf.clear();
    buf.extend_from_slice(MAGIC_AUDIO);
    buf.extend_from_slice(&h.stream_id.to_le_bytes());
    buf.push(h.codec as u8);
    buf.push(h.channels);
    buf.extend_from_slice(&h.sample_rate.to_le_bytes());
    buf.push(nframes);
}

pub fn write_frame(buf: &mut Vec<u8>, seq: u32, flags: u8, data: &[u8]) {
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.push(flags);
    buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
    buf.extend_from_slice(data);
}

pub fn parse_audio(buf: &[u8]) -> Option<(AudioHeader, Vec<Frame<'_>>)> {
    if buf.len() < AUDIO_HEADER_LEN || &buf[0..4] != MAGIC_AUDIO {
        return None;
    }
    let h = AudioHeader {
        stream_id: u32_at(buf, 4),
        codec: Codec::from_u8(buf[8])?,
        channels: buf[9],
        sample_rate: u32_at(buf, 10),
    };
    if h.channels == 0 || h.channels > 2 || h.sample_rate < 8000 || h.sample_rate > 384_000 {
        return None;
    }
    let n = buf[14] as usize;
    let mut frames = Vec::with_capacity(n);
    let mut p = AUDIO_HEADER_LEN;
    for _ in 0..n {
        if p + FRAME_HEADER_LEN > buf.len() {
            return None;
        }
        let seq = u32_at(buf, p);
        let flags = buf[p + 4];
        let len = u16::from_le_bytes([buf[p + 5], buf[p + 6]]) as usize;
        p += FRAME_HEADER_LEN;
        if p + len > buf.len() {
            return None;
        }
        frames.push(Frame { seq, flags, data: &buf[p..p + len] });
        p += len;
    }
    Some((h, frames))
}

pub struct Beacon {
    pub receiver_id: u64,
    pub audio_port: u16,
    pub name: String,
}

pub fn write_beacon(b: &Beacon) -> Vec<u8> {
    let name = &b.name.as_bytes()[..b.name.len().min(200)];
    let mut v = Vec::with_capacity(15 + name.len());
    v.extend_from_slice(MAGIC_BEACON);
    v.extend_from_slice(&b.receiver_id.to_le_bytes());
    v.extend_from_slice(&b.audio_port.to_le_bytes());
    v.push(name.len() as u8);
    v.extend_from_slice(name);
    v
}

pub fn parse_beacon(buf: &[u8]) -> Option<Beacon> {
    if buf.len() < 15 || &buf[0..4] != MAGIC_BEACON {
        return None;
    }
    let len = buf[14] as usize;
    let name = buf.get(15..15 + len)?;
    Some(Beacon {
        receiver_id: u64::from_le_bytes(buf[4..12].try_into().ok()?),
        audio_port: u16::from_le_bytes([buf[12], buf[13]]),
        name: String::from_utf8_lossy(name).into_owned(),
    })
}

/// Ping and pong carry the sender's send time (µs) so RTT needs no bookkeeping.
pub fn write_ping(magic: &[u8; 4], t_us: u64) -> [u8; 12] {
    let mut v = [0u8; 12];
    v[0..4].copy_from_slice(magic);
    v[4..12].copy_from_slice(&t_us.to_le_bytes());
    v
}

pub fn parse_ping(magic: &[u8; 4], buf: &[u8]) -> Option<u64> {
    if buf.len() != 12 || &buf[0..4] != magic {
        return None;
    }
    Some(u64::from_le_bytes(buf[4..12].try_into().ok()?))
}

/// Receiver -> sender, once per second. Counters are cumulative for the stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub stream_id: u32,
    pub received: u32,
    pub recovered: u32,
    pub concealed: u32,
    pub underruns: u32,
    pub target_us: u32,
    pub fill_us: u32,
    pub device_us: u32,
}

pub fn write_report(r: &Report) -> [u8; 36] {
    let mut v = [0u8; 36];
    v[0..4].copy_from_slice(MAGIC_REPORT);
    let fields = [
        r.stream_id, r.received, r.recovered, r.concealed, r.underruns, r.target_us, r.fill_us,
        r.device_us,
    ];
    for (i, f) in fields.iter().enumerate() {
        v[4 + i * 4..8 + i * 4].copy_from_slice(&f.to_le_bytes());
    }
    v
}

pub fn parse_report(buf: &[u8]) -> Option<Report> {
    if buf.len() != 36 || &buf[0..4] != MAGIC_REPORT {
        return None;
    }
    let f = |i: usize| u32_at(buf, 4 + i * 4);
    Some(Report {
        stream_id: f(0),
        received: f(1),
        recovered: f(2),
        concealed: f(3),
        underruns: f(4),
        target_us: f(5),
        fill_us: f(6),
        device_us: f(7),
    })
}

/// Sender -> receiver, once per second: who is sending and its share of the latency.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Info {
    pub stream_id: u32,
    /// Capture + framing delay on the sender, µs.
    pub sender_us: u32,
    pub rtt_us: u32,
    /// `engine::LinkKind` as seen from the sender, encoded by `link_to_u8`.
    pub link: u8,
    pub name: String,
}

pub fn write_info(i: &Info) -> Vec<u8> {
    let name = &i.name.as_bytes()[..i.name.len().min(200)];
    let mut v = Vec::with_capacity(18 + name.len());
    v.extend_from_slice(MAGIC_INFO);
    v.extend_from_slice(&i.stream_id.to_le_bytes());
    v.extend_from_slice(&i.sender_us.to_le_bytes());
    v.extend_from_slice(&i.rtt_us.to_le_bytes());
    v.push(i.link);
    v.push(name.len() as u8);
    v.extend_from_slice(name);
    v
}

pub fn parse_info(buf: &[u8]) -> Option<Info> {
    if buf.len() < 18 || &buf[0..4] != MAGIC_INFO {
        return None;
    }
    let name = buf.get(18..18 + buf[17] as usize)?;
    Some(Info {
        stream_id: u32_at(buf, 4),
        sender_us: u32_at(buf, 8),
        rtt_us: u32_at(buf, 12),
        link: buf[16],
        name: String::from_utf8_lossy(name).into_owned(),
    })
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Signed distance a - b with wrap-around.
pub fn seq_diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_roundtrip() {
        let h = AudioHeader { stream_id: 7, codec: Codec::Pcm16, channels: 2, sample_rate: 48000 };
        let mut buf = Vec::new();
        write_audio_header(&mut buf, &h, 2);
        write_frame(&mut buf, 10, 0, &[1, 2, 3]);
        write_frame(&mut buf, 9, FLAG_REDUNDANT, &[4]);
        let (h2, frames) = parse_audio(&buf).unwrap();
        assert_eq!(h, h2);
        assert_eq!(frames[0], Frame { seq: 10, flags: 0, data: &[1, 2, 3] });
        assert_eq!(frames[1], Frame { seq: 9, flags: FLAG_REDUNDANT, data: &[4] });
        assert!(parse_audio(&buf[..buf.len() - 1]).is_none());
    }

    #[test]
    fn control_roundtrip() {
        let b = parse_beacon(&write_beacon(&Beacon {
            receiver_id: 42,
            audio_port: 1234,
            name: "Mac".into(),
        }))
        .unwrap();
        assert_eq!((b.receiver_id, b.audio_port, b.name.as_str()), (42, 1234, "Mac"));
        assert_eq!(parse_ping(MAGIC_PING, &write_ping(MAGIC_PING, 99)), Some(99));
        assert_eq!(parse_ping(MAGIC_PONG, &write_ping(MAGIC_PING, 99)), None);
        let r = Report { stream_id: 1, received: 2, concealed: 3, fill_us: 4, ..Default::default() };
        assert_eq!(parse_report(&write_report(&r)), Some(r));
        let i = Info { stream_id: 5, sender_us: 11_000, rtt_us: 300, link: 2, name: "PC".into() };
        assert_eq!(parse_info(&write_info(&i)), Some(i));
    }

    #[test]
    fn wrapping_seq() {
        assert_eq!(seq_diff(1, u32::MAX), 2);
        assert_eq!(seq_diff(u32::MAX, 1), -2);
    }
}
