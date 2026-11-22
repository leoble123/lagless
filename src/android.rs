//! Android-only helpers for the engine. The app's UI, Wi-Fi/wake locks and the playback service
//! are Java (android/app); android/src/lib.rs is the JNI bridge.

/// Write a line to logcat (tag "lagless").
pub fn log(msg: &str) {
    extern "C" {
        fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
    }
    let text = std::ffi::CString::new(msg.replace('\0', " ")).unwrap_or_default();
    // ANDROID_LOG_WARN = 5
    unsafe { __android_log_write(5, c"lagless".as_ptr(), text.as_ptr()) };
}

/// "SM-S721B"… from the system properties (fallback when Java doesn't pass a name).
pub fn device_name() -> Option<String> {
    let out = std::process::Command::new("getprop").arg("ro.product.model").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
