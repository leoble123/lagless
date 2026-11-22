//! Android glue: the few things only Java APIs can do, called through JNI.
//!
//! * Wi-Fi "low latency" lock (Android 10+; high-perf before): stops the phone's Wi-Fi power
//!   saving, which otherwise batches packets every ~100 ms and is the classic cause of choppy
//!   audio on phones.
//! * Multicast lock: some phones drop broadcast packets without it (discovery).
//! * Partial wake lock + a foreground service (app.lagless.LaglessService, in the APK): keep
//!   playing with the screen off.
//! * Opening a link in the browser (updates).

use jni::objects::{JObject, JValue};
use jni::JNIEnv;

fn with_env<T>(f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> anyhow::Result<T> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast())? };
    let mut env = vm.attach_current_thread()?;
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    let r = f(&mut env, &activity);
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    Ok(r?)
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
    with_env(|env, activity| {
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
    with_env(|env, activity| {
        if sdk_int(env) < 33 {
            return Ok(());
        }
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
