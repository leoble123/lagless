//! lagless: low-latency audio streaming between computers and phones.
//!
//! The engine and UI live here so the desktop binary (src/main.rs) and the Android app
//! (android/) share one codebase.

pub mod audio;
pub mod aware;
pub mod boost;
pub mod codec;
pub mod control;
pub mod engine;
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
