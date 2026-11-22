//! Android entry point: NativeActivity loads liblagless_android.so and calls `android_main`.
//!
//! Anything that goes wrong while starting (a panic, or the UI failing to initialise) opens a
//! crash screen with the details instead of the app silently closing.

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    use std::sync::{Arc, Mutex};
    let last_panic: Arc<Mutex<Option<String>>> = Default::default();
    let lp = last_panic.clone();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let msg = format!(
            "panic in thread '{thread}': {info}\n\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        lagless::android::log(&msg);
        *lp.lock().unwrap() = Some(msg);
    }));
    let version = env!("CARGO_PKG_VERSION");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lagless::gui::run_android(app)));
    let report = match r {
        Ok(Ok(())) => return,
        Ok(Err(e)) => format!("The window couldn't be created: {e}"),
        Err(_) => last_panic.lock().unwrap().take().unwrap_or_else(|| "panic (no details)".into()),
    };
    let device = lagless::android::device_name().unwrap_or_default();
    lagless::android::show_crash(&format!("lagless {} (Android lib {version}) on {device}\n\n{report}", lagless::update::VERSION));
    // Give the crash screen a moment to launch before this process goes away.
    std::thread::sleep(std::time::Duration::from_millis(500));
}
