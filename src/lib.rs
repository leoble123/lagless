//! lagless: low-latency audio streaming between computers and phones.
//!
//! The engine lives here so the desktop app (src/main.rs + gui) and the Android app (android/,
//! a native Java UI calling into this through JNI) share one codebase.

pub mod audio;
pub mod aware;
pub mod boost;
pub mod codec;
pub mod control;
pub mod engine;
#[cfg(not(target_os = "android"))]
pub mod gui;
pub mod mic_access;
pub mod net;
pub mod playout;
pub mod protocol;
pub mod receiver;
pub mod sender;
pub mod update;
pub mod voice;
pub mod yamnet;
#[cfg(target_os = "android")]
pub mod android;
