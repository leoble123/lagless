//! Android entry point: NativeActivity loads liblagless_android.so and calls `android_main`.

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: winit::platform::android::activity::AndroidApp) {
    if let Err(e) = lagless::gui::run_android(app) {
        eprintln!("lagless: {e}");
    }
}
