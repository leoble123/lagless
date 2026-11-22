//! JNI bridge between the Android UI (android/app, Java) and the lagless engine.
//!
//! Java calls `Engine.start/stop/setVolume/status` (app.lagless.Engine). Every entry point catches
//! panics and turns them into an error string, so a Rust problem shows up as text in the app
//! instead of killing it.

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jfloat, jstring};
use jni::JNIEnv;
use lagless::engine::{Phase, Shared};
use lagless::playout::Volume;
use lagless::{protocol, receiver, update};
use std::sync::{Arc, Mutex, OnceLock};

struct Running {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

static ENGINE: Mutex<Option<Running>> = Mutex::new(None);
static UPDATER: OnceLock<update::Updater> = OnceLock::new();
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

fn log(msg: &str) {
    #[cfg(target_os = "android")]
    lagless::android::log(msg);
    #[cfg(not(target_os = "android"))]
    eprintln!("lagless: {msg}");
}

fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn jstr(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).map(|j| j.into_raw()).unwrap_or(std::ptr::null_mut())
}

/// Start receiving. `context` is the Application (the audio backend needs it); `name` is how
/// this phone shows up on the PC. Returns null on success, or an error message.
#[no_mangle]
pub extern "system" fn Java_app_lagless_Engine_start<'a>(mut env: JNIEnv<'a>, _class: JClass<'a>, context: JObject<'a>, name: JString<'a>, volume: jfloat) -> jstring {
    let result = guard(Err("panic while starting (see log)".to_string()), || -> Result<(), String> {
        static INIT: OnceLock<()> = OnceLock::new();
        let mut init_err = None;
        INIT.get_or_init(|| {
            std::panic::set_hook(Box::new(|info| {
                let msg = format!("panic in '{}': {info}", std::thread::current().name().unwrap_or("?"));
                log(&msg);
                *LAST_PANIC.lock().unwrap() = Some(msg);
            }));
            // cpal's Android (Oboe) backend finds the JVM and app context through ndk-context.
            match (env.get_java_vm(), env.new_global_ref(&context)) {
                (Ok(vm), Ok(ctx)) => unsafe {
                    ndk_context::initialize_android_context(vm.get_java_vm_pointer().cast(), ctx.as_obj().as_raw().cast());
                    std::mem::forget(ctx); // must live for the whole process
                },
                (a, b) => init_err = Some(format!("JNI setup failed: {:?} {:?}", a.err(), b.err())),
            }
        });
        if let Some(e) = init_err {
            return Err(e);
        }
        let name: String = env.get_string(&name).map(|s| s.into()).unwrap_or_default();
        let mut engine = ENGINE.lock().unwrap();
        if engine.is_some() {
            return Ok(());
        }
        let shared = Shared::new();
        shared.volume.set(Volume::slider_to_gain(volume));
        let sh = shared.clone();
        let opts = receiver::Options {
            port: protocol::AUDIO_PORT,
            device: None,
            min_latency_ms: 2.0,
            max_latency_ms: 150.0,
            buffer_frames: 64,
            name: Some(name).filter(|n| !n.trim().is_empty()),
            // Only for the desktop JNI test harness (no sound card there).
            null_output: std::env::var_os("LAGLESS_NULL_OUTPUT").is_some(),
            spike_memory_secs: 20,
        };
        let handle = std::thread::Builder::new()
            .name("engine".into())
            .spawn(move || {
                if let Err(e) = receiver::run(opts, sh.clone()) {
                    log(&format!("engine: {e:#}"));
                    sh.update(|st| st.phase = Phase::Error(format!("{e:#}")));
                }
            })
            .map_err(|e| e.to_string())?;
        *engine = Some(Running { shared, handle: Some(handle) });
        UPDATER.get_or_init(|| update::Updater::start(true));
        Ok(())
    });
    match result {
        Ok(()) => std::ptr::null_mut(),
        Err(e) => jstr(&mut env, &e),
    }
}

#[no_mangle]
pub extern "system" fn Java_app_lagless_Engine_stop(_env: JNIEnv, _class: JClass) {
    guard((), || {
        if let Some(mut r) = ENGINE.lock().unwrap().take() {
            r.shared.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(h) = r.handle.take() {
                let _ = h.join();
            }
        }
    });
}

/// `position` is the slider (0..1); the engine maps it to a gain like the desktop app does.
#[no_mangle]
pub extern "system" fn Java_app_lagless_Engine_setVolume(_env: JNIEnv, _class: JClass, position: jfloat) {
    guard((), || {
        if let Some(r) = ENGINE.lock().unwrap().as_ref() {
            r.shared.volume.set(Volume::slider_to_gain(position));
        }
    });
}

/// Everything the screen shows, as JSON.
#[no_mangle]
pub extern "system" fn Java_app_lagless_Engine_status<'a>(mut env: JNIEnv<'a>, _class: JClass<'a>) -> jstring {
    let json = guard(r#"{"phase":"error","error":"status panicked"}"#.to_string(), || {
        let engine = ENGINE.lock().unwrap();
        let Some(r) = engine.as_ref() else { return r#"{"phase":"stopped"}"#.to_string() };
        let s = r.shared.snapshot();
        let events = r.shared.take_events();
        let (phase, error) = match &s.phase {
            Phase::Starting => ("starting", String::new()),
            Phase::Searching => ("waiting", String::new()),
            Phase::Idle => ("idle", String::new()),
            Phase::Streaming => ("playing", String::new()),
            Phase::Error(e) => ("error", e.clone()),
        };
        let error = LAST_PANIC.lock().unwrap().clone().unwrap_or(error);
        let (update_version, update_url) = match UPDATER.get().map(|u| (u.state(), u.download_url())) {
            Some((update::State::Available(v), Some(url))) => (v, url),
            _ => (String::new(), String::new()),
        };
        serde_json::json!({
            "phase": phase,
            "error": error,
            "event": events.last().cloned().unwrap_or_default(),
            "peer": s.peer_name,
            "addr": s.peer_addr,
            "link": s.link.label(),
            "link_name": s.link_name,
            "codec": s.codec,
            "rate": s.sample_rate,
            "lossless": s.lossless,
            "latency_ms": s.latency_ms,
            "buffer_ms": s.buffer_ms,
            "target_ms": s.target_ms,
            "jitter_ms": s.jitter_ms,
            "device_ms": s.device_ms,
            "fixed": s.fixed,
            "lost": s.lost,
            "underruns": s.underruns,
            "version": update::VERSION,
            "update_version": update_version,
            "update_url": update_url,
        })
        .to_string()
    });
    jstr(&mut env, &json)
}
