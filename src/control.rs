//! Local control API, so other apps on this computer (like the Nova launcher) can show lagless's
//! status and drive it.
//!
//! HTTP/1.1 on 127.0.0.1 only, on a random port. Every request needs the bearer token that is
//! written, together with the port, to `control.json` next to `settings.json`. Anything that looks
//! like it comes from a web page (an `Origin` header, or a `Host` that isn't loopback) is refused,
//! so a website can't reach it even by guessing the port.
//!
//! - `GET  /v1/status`  → live status (JSON)
//! - `POST /v1/volume`  `{"volume": 0.0..1.0}` (slider position, same as the app's slider)
//! - `POST /v1/mute`    `{"muted": bool}`
//! - `POST /v1/mic`     `{"on": bool}` (PC: "Use Mac mic"; Mac: "Send mic to PC")
//! - `POST /v1/pause`   `{"paused": bool}` (stops/starts the stream without quitting)

use crate::engine::{Phase, Shared, Status};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Volume(f32),
    Mute(bool),
    Mic(bool),
    Pause(bool),
}

/// The parts of the app's state that live in the GUI rather than the engine.
#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub mode: &'static str,
    pub paused: bool,
    pub volume: f32,
    pub mic_enabled: bool,
    pub mic_problem: Option<String>,
}

pub struct Control {
    token: String,
    file: Option<PathBuf>,
    app: Mutex<AppState>,
    engine: Mutex<Option<Arc<Shared>>>,
    mic: Mutex<Option<Arc<Shared>>>,
    commands: Mutex<Vec<Command>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Control {
    /// Starts the server and writes `control.json` into `dir`. `wake` is called after a command
    /// is queued so the app picks it up even while its window is minimized.
    pub fn start(dir: Option<PathBuf>, wake: impl Fn() + Send + Sync + 'static) -> Option<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let port = listener.local_addr().ok()?.port();
        let control = Arc::new(Control {
            token: new_token(),
            file: dir.map(|d| d.join("control.json")),
            app: Mutex::new(AppState::default()),
            engine: Mutex::new(None),
            mic: Mutex::new(None),
            commands: Mutex::new(Vec::new()),
            wake: Box::new(wake),
        });
        if let Some(f) = &control.file {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let info = json!({ "port": port, "token": control.token, "pid": std::process::id(), "version": crate::update::VERSION });
            let _ = std::fs::write(f, info.to_string());
        }
        let me = control.clone();
        std::thread::Builder::new()
            .name("control".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    me.serve(stream);
                }
            })
            .ok()?;
        Some(control)
    }

    /// Called by the app every frame (cheap: clones a few Arcs).
    pub fn publish(&self, app: AppState, engine: &Arc<Shared>, mic: Option<&Arc<Shared>>) {
        *self.app.lock().unwrap() = app;
        *self.engine.lock().unwrap() = Some(engine.clone());
        *self.mic.lock().unwrap() = mic.cloned();
    }

    pub fn take_commands(&self) -> Vec<Command> {
        std::mem::take(&mut *self.commands.lock().unwrap())
    }

    /// Removes `control.json` so other apps stop trying to connect.
    pub fn shutdown(&self) {
        if let Some(f) = &self.file {
            let _ = std::fs::remove_file(f);
        }
    }

    /// Built from the engines directly, so it stays live while the window is minimized.
    pub fn status_json(&self) -> Value {
        let app = self.app.lock().unwrap().clone();
        let st = self.engine.lock().unwrap().as_ref().map(|e| e.snapshot()).unwrap_or_default();
        let mic = self.mic.lock().unwrap().as_ref().map(|m| m.snapshot());
        let error = match &st.phase {
            Phase::Error(e) => Some(e.clone()),
            _ => None,
        };
        json!({
            "version": crate::update::VERSION,
            "mode": app.mode,
            "paused": app.paused,
            // A paused engine is stopped, so its last status would still say "streaming".
            "phase": if app.paused { "paused" } else { phase_name(&st) },
            "error": error,
            "peer": st.peer_name,
            "peerAddr": st.peer_addr,
            "link": if st.link_name.is_empty() { "" } else { st.link.label() },
            "linkName": st.link_name,
            "codec": st.codec,
            "sampleRate": st.sample_rate,
            "lossless": st.lossless,
            "latencyMs": if app.paused { 0.0 } else { st.latency_ms },
            "rttMs": st.rtt_ms,
            "lossPct": st.loss_pct,
            "bufferMs": st.buffer_ms,
            "volume": app.volume,
            "muted": app.volume <= 0.0,
            "mic": {
                "enabled": app.mic_enabled,
                "active": mic.as_ref().is_some_and(|m| m.phase == Phase::Streaming),
                "latencyMs": mic.as_ref().map(|m| m.latency_ms).unwrap_or(0.0),
                "problem": app.mic_problem,
            },
        })
    }

    fn serve(&self, stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let (code, body) = match read_request(&stream) {
            Some(req) => self.handle(&req),
            None => (400, json!({ "error": "bad request" })),
        };
        respond(stream, code, &body);
    }

    fn handle(&self, req: &Request) -> (u16, Value) {
        // A browser always sends Origin on cross-site requests, and a DNS-rebinding page would
        // arrive with its own Host name. Neither is a legitimate local app.
        if req.header("origin").is_some() {
            return (403, json!({ "error": "not available to web pages" }));
        }
        let host = req.header("host").unwrap_or("");
        if !(host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.is_empty()) {
            return (403, json!({ "error": "bad host" }));
        }
        let auth = req.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or("");
        if !constant_time_eq(auth.trim().as_bytes(), self.token.as_bytes()) {
            return (401, json!({ "error": "missing or wrong token (see control.json)" }));
        }
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/v1/status") => (200, self.status_json()),
            ("POST", path) => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
                let cmd = match path {
                    "/v1/volume" => body["volume"].as_f64().map(|v| Command::Volume(v.clamp(0.0, 1.0) as f32)),
                    "/v1/mute" => body["muted"].as_bool().map(Command::Mute),
                    "/v1/mic" => body["on"].as_bool().map(Command::Mic),
                    "/v1/pause" => body["paused"].as_bool().map(Command::Pause),
                    _ => return (404, json!({ "error": "not found" })),
                };
                match cmd {
                    Some(c) => {
                        self.commands.lock().unwrap().push(c);
                        (self.wake)();
                        (200, json!({ "ok": true }))
                    }
                    None => (400, json!({ "error": "missing or wrong value in body" })),
                }
            }
            _ => (404, json!({ "error": "not found" })),
        }
    }
}

fn phase_name(st: &Status) -> &'static str {
    match st.phase {
        Phase::Starting => "starting",
        Phase::Searching => "searching",
        Phase::Idle => "idle",
        Phase::Streaming => "streaming",
        Phase::Error(_) => "error",
    }
}

/// 128 bits from the OS-seeded hasher keys, mixed with time and pid through SHA-256.
fn new_token() -> String {
    let mut h = Sha256::new();
    for _ in 0..4 {
        let mut s = std::collections::hash_map::RandomState::new().build_hasher();
        s.write_u64(0x6c61_676c_6573_73);
        h.update(s.finish().to_le_bytes());
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.finalize().iter().take(18).map(|b| format!("{b:02x}")).collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut r = BufReader::new(stream.take(16 * 1024));
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).ok()? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > 4096 {
        return None;
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    Some(Request { method, path, headers, body })
}

fn respond(mut stream: TcpStream, code: u16, body: &Value) {
    let text = body.to_string();
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn call(port: u16, raw: &str) -> (u16, Value) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let code = out[9..12].parse().unwrap();
        let body = out.split("\r\n\r\n").nth(1).unwrap_or("null");
        (code, serde_json::from_str(body).unwrap_or(Value::Null))
    }

    #[test]
    fn serves_status_and_queues_commands_only_with_token() {
        let dir = std::env::temp_dir().join(format!("lagless-control-test-{}", std::process::id()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = wakes.clone();
        let c = Control::start(Some(dir.clone()), move || {
            w.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        let info: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("control.json")).unwrap()).unwrap();
        let port = info["port"].as_u64().unwrap() as u16;
        let token = info["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 36);

        c.publish(AppState { mode: "send", volume: 0.5, ..Default::default() }, &Shared::new(), None);

        let (code, _) = call(port, "GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert_eq!(code, 401);

        let (code, _) = call(port, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: https://evil.example\r\nAuthorization: Bearer {token}\r\n\r\n"));
        assert_eq!(code, 403);

        let (code, _) = call(port, &format!("GET /v1/status HTTP/1.1\r\nHost: evil.example:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"));
        assert_eq!(code, 403);

        let (code, body) = call(port, &format!("GET /v1/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"));
        assert_eq!(code, 200);
        assert_eq!(body["mode"], "send");
        assert_eq!(body["volume"], 0.5);
        assert_eq!(body["phase"], "starting");

        let b = r#"{"volume": 1.7}"#;
        let (code, _) = call(port, &format!("POST /v1/volume HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        assert_eq!(code, 200);
        let b = r#"{"paused": true}"#;
        call(port, &format!("POST /v1/pause HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        let b = r#"{"on": "yes"}"#;
        let (code, _) = call(port, &format!("POST /v1/mic HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{b}", b.len()));
        assert_eq!(code, 400);

        assert_eq!(c.take_commands(), vec![Command::Volume(1.0), Command::Pause(true)]);
        assert_eq!(wakes.load(Ordering::SeqCst), 2);

        c.shutdown();
        assert!(!dir.join("control.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
