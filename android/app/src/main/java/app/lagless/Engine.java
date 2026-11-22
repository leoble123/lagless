package app.lagless;

import android.content.Context;

/** The Rust engine (liblagless_android.so, android/src/lib.rs). */
final class Engine {
    /** Start receiving. Returns null on success, or an error message. */
    static native String start(Context appContext, String deviceName, float volume);
    static native void stop();
    /** Slider position 0..1. */
    static native void setVolume(float position);
    /** Everything the screen shows, as JSON. */
    static native String status();
}
