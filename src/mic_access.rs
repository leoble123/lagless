//! Why a microphone might be silent, in words a person can act on.
//!
//! On macOS a denied microphone doesn't produce an error anywhere: CoreAudio just never hands
//! the app any audio. So we ask the privacy system directly and say what to do.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub enum Access {
    NotAsked,
    Denied,
    Granted,
    Unknown,
}

/// macOS: `[AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]`.
#[cfg(target_os = "macos")]
pub fn status() -> Access {
    use std::ffi::{c_char, c_void};
    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeAudio: *const c_void;
    }
    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }
    unsafe {
        let cls = objc_getClass(c"AVCaptureDevice".as_ptr());
        if cls.is_null() {
            return Access::Unknown;
        }
        let sel = sel_registerName(c"authorizationStatusForMediaType:".as_ptr());
        // objc_msgSend must be called through a pointer of the method's exact type (arm64 ABI).
        let send: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void) -> isize =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        match send(cls, sel, AVMediaTypeAudio) {
            0 => Access::NotAsked,
            1 | 2 => Access::Denied, // restricted / denied
            3 => Access::Granted,
            _ => Access::Unknown,
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn status() -> Access {
    Access::Unknown
}

/// What to tell the person when the mic has produced nothing for a couple of seconds.
pub fn silent_mic_message() -> String {
    match status() {
        Access::Denied => "macOS is blocking the microphone for lagless. Turn lagless on in System Settings → Privacy & Security → Microphone, then quit and reopen lagless.".into(),
        Access::NotAsked => "Waiting for microphone permission — look for a macOS prompt and click Allow.".into(),
        Access::Granted => "The microphone isn't sending any audio. Pick it in lagless settings (Microphone), or check System Settings → Sound → Input.".into(),
        Access::Unknown => "The microphone isn't sending any audio. Check that lagless is allowed to use it and that the right input is selected.".into(),
    }
}

/// Open the Microphone page of the OS privacy settings.
pub fn open_settings() {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone").spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/c", "start", "ms-settings:privacy-microphone"]).spawn();
}
