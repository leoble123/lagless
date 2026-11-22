package app.lagless;

import android.content.Context;

/**
 * Drives the real JNI bridge (liblagless_android.so built for the host) the way MainActivity
 * does: start, poll status, change volume, stop. See android/test-harness/run.sh.
 */
public class BridgeTest {
    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        String err = Engine.start(new Context(), "TestPhone", 0.5f);
        System.out.println("start -> " + (err == null ? "ok" : err));
        String again = Engine.start(new Context(), "TestPhone", 0.5f);
        System.out.println("start again -> " + (again == null ? "ok (no-op)" : again));
        long end = System.currentTimeMillis() + Integer.parseInt(args[1]) * 1000L;
        String last = "";
        while (System.currentTimeMillis() < end) {
            last = Engine.status();
            Thread.sleep(250);
        }
        System.out.println("status -> " + last);
        Engine.setVolume(0.2f);
        Engine.stop();
        System.out.println("after stop -> " + Engine.status());
    }
}
