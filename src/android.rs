//! Android glue: the few things only Java APIs can do, called through JNI.
//!
//! * Wi-Fi "low latency" lock (Android 10+; high-perf before): stops the phone's Wi-Fi power
//!   saving, which otherwise batches packets every ~100 ms and is the classic cause of choppy
//!   audio on phones.
//! * Multicast lock: some phones drop broadcast packets without it (discovery).
//! * Partial wake lock + a foreground service (app.lagless.LaglessService, in the APK): keep
//!   playing with the screen off.
//! * Opening a link in the browser (updates).

use jni::objects::{GlobalRef, JObject, JValue};
use jni::JNIEnv;
use std::sync::OnceLock;

/// The NativeActivity (android-activity registers the *Application* as ndk_context's context;
/// a few calls, like asking for a permission, need the Activity itself).
static ACTIVITY: OnceLock<GlobalRef> = OnceLock::new();

pub fn set_activity(activity_ptr: *mut std::ffi::c_void) {
    let _ = with_env(|env, _| {
        let local = unsafe { JObject::from_raw(activity_ptr.cast()) };
        let global = env.new_global_ref(&local)?;
        let _ = ACTIVITY.set(global);
        Ok(())
    });
}

/// Runs `f` with a JNI env and the app context (the Application). Java exceptions become errors
/// and are cleared, so a failing call can never take the app down.
fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> anyhow::Result<T> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast())? };
    let mut env = vm.attach_current_thread()?;
    let app = unsafe { JObject::from_raw(ctx.context().cast()) };
    let r = f(&mut env, &app);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    Ok(r?)
}

// ---------------------------------------------------------------- startup watchdog
//
// An Android app that stops processing events for ~5 s gets "App not responding" and nothing
// else. To find out where lagless gets stuck, the UI thread leaves breadcrumbs and a watchdog
// thread reports a stall (step + this app's own recent log lines) on the crash screen.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

static STEP: Mutex<&'static str> = Mutex::new("starting");
static STARTED: OnceLock<std::time::Instant> = OnceLock::new();
static FRAME_START_MS: AtomicU64 = AtomicU64::new(0);
static IN_FRAME: AtomicBool = AtomicBool::new(false);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);

fn now_ms() -> u64 {
    STARTED.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// Breadcrumb: what the UI thread is doing right now.
pub fn step(s: &'static str) {
    *STEP.lock().unwrap() = s;
    log(&format!("step: {s}"));
}

pub fn frame_begin() {
    FRAME_START_MS.store(now_ms(), Relaxed);
    IN_FRAME.store(true, Relaxed);
}

pub fn frame_end() {
    IN_FRAME.store(false, Relaxed);
    if FRAMES.fetch_add(1, Relaxed) == 0 {
        step("first frame done");
    }
}

/// Watch for the UI thread getting stuck: no first frame within 8 s, or a frame taking 4 s.
pub fn start_watchdog() {
    now_ms();
    let _ = std::thread::Builder::new().name("watchdog".into()).spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let t = now_ms();
        let stuck_in_frame = IN_FRAME.load(Relaxed) && t.saturating_sub(FRAME_START_MS.load(Relaxed)) > 4_000;
        let no_first_frame = FRAMES.load(Relaxed) == 0 && t > 8_000;
        if (stuck_in_frame || no_first_frame) && !REPORTED.swap(true, Relaxed) {
            let what = if stuck_in_frame { "stuck inside a frame" } else { "no first frame after 8 s" };
            let step = *STEP.lock().unwrap();
            show_crash(&format!(
                "lagless {} on {}\n\nThe app froze ({what}).\nLast step: {step}\nFrames drawn: {}\nTime since start: {:.1} s\n\nRecent log:\n{}",
                crate::update::VERSION,
                device_name().unwrap_or_default(),
                FRAMES.load(Relaxed),
                t as f64 / 1000.0,
                own_log()
            ));
        }
    });
}

/// This app's own recent logcat lines (apps may read their own logs without any permission).
pub fn own_log() -> String {
    let pid = std::process::id().to_string();
    std::process::Command::new("logcat")
        .args(["-d", "-t", "150", "--pid", &pid])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_else(|e| format!("(couldn't read log: {e})"))
}

/// Do Java calls off the UI thread, so the UI thread can never end up waiting on Android's
/// main thread while Android's main thread waits on it.
pub fn in_background(name: &'static str, f: impl FnOnce() -> anyhow::Result<()> + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
        if let Err(e) = f() {
            log(&format!("{name}: {e}"));
        }
    });
}

/// Write a line to logcat (tag "lagless").
pub fn log(msg: &str) {
    extern "C" {
        fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
    }
    let text = std::ffi::CString::new(msg.replace('\0', " ")).unwrap_or_default();
    // ANDROID_LOG_WARN = 5
    unsafe { __android_log_write(5, c"lagless".as_ptr(), text.as_ptr()) };
}

/// Open CrashActivity (its own process, so it survives this one exiting) showing `text`.
pub fn show_crash(text: &str) {
    log(text);
    let r = with_env(|env, app| {
        let loader = env.call_method(app, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string("app.lagless.CrashActivity")?;
        let class = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
        let intent = env.new_object("android/content/Intent", "(Landroid/content/Context;Ljava/lang/Class;)V", &[JValue::Object(app), JValue::Object(&class)])?;
        let key = env.new_string("text")?;
        let val = env.new_string(text)?;
        env.call_method(&intent, "putExtra", "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;", &[JValue::Object(&key), JValue::Object(&val)])?;
        // FLAG_ACTIVITY_NEW_TASK | FLAG_ACTIVITY_CLEAR_TASK
        env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1000_0000 | 0x0000_8000)])?;
        env.call_method(app, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
        Ok(())
    });
    if let Err(e) = r {
        log(&format!("couldn't show the crash screen: {e}"));
    }
}

fn system_service<'a>(env: &mut JNIEnv<'a>, activity: &JObject, name: &str) -> jni::errors::Result<JObject<'a>> {
    let n = env.new_string(name)?;
    env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&n)])?.l()
}

fn sdk_int(env: &mut JNIEnv) -> i32 {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I").and_then(|v| v.i()).unwrap_or(0)
}

/// Keep Wi-Fi and the CPU awake and responsive while lagless runs. The locks are deliberately
/// leaked (global refs): they're released when the process ends.
pub fn keep_network_fast() -> anyhow::Result<()> {
    with_env(|env, activity| {
        let wifi = system_service(env, activity, "wifi")?;
        let tag = env.new_string("lagless")?;
        // WIFI_MODE_FULL_LOW_LATENCY = 4 (API 29+), WIFI_MODE_FULL_HIGH_PERF = 3.
        let mode = if sdk_int(env) >= 29 { 4 } else { 3 };
        let lock = env
            .call_method(&wifi, "createWifiLock", "(ILjava/lang/String;)Landroid/net/wifi/WifiManager$WifiLock;", &[JValue::Int(mode), JValue::Object(&tag)])?
            .l()?;
        env.call_method(&lock, "acquire", "()V", &[])?;
        std::mem::forget(env.new_global_ref(&lock)?);

        let mlock = env
            .call_method(&wifi, "createMulticastLock", "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;", &[JValue::Object(&tag)])?
            .l()?;
        env.call_method(&mlock, "acquire", "()V", &[])?;
        std::mem::forget(env.new_global_ref(&mlock)?);

        let power = system_service(env, activity, "power")?;
        // PARTIAL_WAKE_LOCK = 1
        let wtag = env.new_string("lagless:audio")?;
        let wake = env
            .call_method(&power, "newWakeLock", "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;", &[JValue::Int(1), JValue::Object(&wtag)])?
            .l()?;
        env.call_method(&wake, "acquire", "()V", &[])?;
        std::mem::forget(env.new_global_ref(&wake)?);
        Ok(())
    })
}

/// Start app.lagless.LaglessService as a foreground service (media playback), so Android keeps
/// the app running and playing when the screen is off or you switch apps.
pub fn start_foreground_service() -> anyhow::Result<()> {
    with_env(|env, app| {
        let activity = ACTIVITY.get().map(|g| g.as_obj()).unwrap_or(app);
        // App classes aren't visible to FindClass from a native thread: use the app's loader.
        let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
        let name = env.new_string("app.lagless.LaglessService")?;
        let class = env.call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?.l()?;
        let intent = env.new_object("android/content/Intent", "(Landroid/content/Context;Ljava/lang/Class;)V", &[JValue::Object(activity), JValue::Object(&class)])?;
        env.call_method(activity, "startForegroundService", "(Landroid/content/Intent;)Landroid/content/ComponentName;", &[JValue::Object(&intent)])?;
        Ok(())
    })
}

/// Ask for the notification permission (Android 13+) so the "playing" notification shows.
pub fn request_notification_permission() -> anyhow::Result<()> {
    with_env(|env, _app| {
        if sdk_int(env) < 33 {
            return Ok(());
        }
        let Some(activity) = ACTIVITY.get().map(|g| g.as_obj()) else { return Ok(()) };
        let perm = env.new_string("android.permission.POST_NOTIFICATIONS")?;
        let arr = env.new_object_array(1, "java/lang/String", &perm)?;
        env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&arr), JValue::Int(1)])?;
        Ok(())
    })
}

pub fn open_url(url: &str) -> anyhow::Result<()> {
    with_env(|env, activity| {
        let s = env.new_string(url)?;
        let uri = env.call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&s)])?.l()?;
        let action = env.new_string("android.intent.action.VIEW")?;
        let intent = env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[JValue::Object(&action), JValue::Object(&uri)])?;
        // FLAG_ACTIVITY_NEW_TASK
        env.call_method(&intent, "addFlags", "(I)Landroid/content/Intent;", &[JValue::Int(0x1000_0000)])?;
        env.call_method(activity, "startActivity", "(Landroid/content/Intent;)V", &[JValue::Object(&intent)])?;
        Ok(())
    })
}

/// "Pixel 8", "SM-S918B"… from the system properties (no permission needed).
pub fn device_name() -> Option<String> {
    let out = std::process::Command::new("getprop").arg("ro.product.model").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
