package app.lagless;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.res.ColorStateList;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.View;
import android.view.Window;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.TextView;

import org.json.JSONObject;

import java.io.PrintWriter;
import java.io.StringWriter;
import java.util.Locale;

/**
 * lagless on Android: this phone receives the PC's audio. Plain Android views (no GPU
 * rendering of our own), dark and quiet in the style of iOS settings; the audio engine is the
 * same Rust code as on the desktop, called through Engine.
 */
public class MainActivity extends Activity {
    // iOS dark palette
    private static final int BG = Color.BLACK;
    private static final int CARD = Color.rgb(28, 28, 30);
    private static final int TEXT = Color.WHITE;
    private static final int DIM = Color.rgb(142, 142, 147);
    private static final int GREEN = Color.rgb(48, 209, 88);
    private static final int AMBER = Color.rgb(255, 214, 10);
    private static final int RED = Color.rgb(255, 69, 58);
    private static final int BLUE = Color.rgb(10, 132, 255);

    private final Handler ui = new Handler(Looper.getMainLooper());
    private boolean libraryLoaded;
    private boolean started;
    private boolean polling;
    private String deviceName;

    private TextView latency, unit, dot, statusLine, hintLine, errorText, volumePct, footer;
    private LinearLayout errorCard;
    private TextView dLink, dFormat, dBuffer, dJitter, dDrops;
    private Button updateButton;
    private String updateUrl = "";
    private SeekBar volume;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        installCrashScreen();
        Window w = getWindow();
        w.setStatusBarColor(BG);
        w.setNavigationBarColor(BG);
        deviceName = deviceName();
        setContentView(buildUi());
        try {
            System.loadLibrary("lagless_android");
            libraryLoaded = true;
        } catch (Throwable t) {
            showError("The audio engine couldn't load on this phone:\n" + t);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (libraryLoaded && !started) {
            started = true;
            if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
                requestPermissions(new String[]{Manifest.permission.POST_NOTIFICATIONS}, 1);
            }
            try {
                startForegroundService(new Intent(this, LaglessService.class));
            } catch (RuntimeException e) {
                // Plays fine without it while the screen is on.
            }
            final float vol = prefs().getFloat("volume", 1f);
            new Thread(() -> {
                String err = Engine.start(getApplicationContext(), deviceName, vol);
                if (err != null) ui.post(() -> showError(err));
            }, "engine-start").start();
        }
        if (libraryLoaded && !polling) {
            polling = true;
            ui.post(poll);
        }
    }

    @Override
    protected void onPause() {
        super.onPause();
        polling = false;
        ui.removeCallbacks(poll);
    }

    @Override
    protected void onDestroy() {
        if (isFinishing() && libraryLoaded) {
            Engine.stop();
            stopService(new Intent(this, LaglessService.class));
        }
        super.onDestroy();
    }

    private final Runnable poll = new Runnable() {
        @Override
        public void run() {
            if (!polling) return;
            try {
                render(new JSONObject(Engine.status()));
            } catch (Throwable t) {
                showError("Status: " + t);
            }
            ui.postDelayed(this, 250);
        }
    };

    // ------------------------------------------------------------------ rendering

    private long lastLatencyUpdate;

    private void render(JSONObject s) {
        String phase = s.optString("phase");
        String peer = s.optString("peer");
        double lat = s.optDouble("latency_ms", 0);
        String error = s.optString("error");

        if (!error.isEmpty()) showError(error);

        if (phase.equals("playing") && lat > 0) {
            // Refresh the big number once a second so it doesn't flicker.
            long now = System.currentTimeMillis();
            if (now - lastLatencyUpdate > 1000) {
                latency.setText(String.valueOf(Math.round(lat)));
                lastLatencyUpdate = now;
            }
            unit.setVisibility(View.VISIBLE);
        } else {
            latency.setText("–");
            unit.setVisibility(View.GONE);
        }

        int col;
        String line;
        String hint = "";
        switch (phase) {
            case "playing":
                col = lat < 25 ? GREEN : lat < 45 ? AMBER : RED;
                line = "From " + peer + " · " + s.optString("link") + " · " + format(s);
                break;
            case "idle":
                col = DIM;
                line = "Connected to " + peer + " — nothing playing";
                break;
            case "error":
                col = RED;
                line = "Something went wrong";
                break;
            default:
                col = DIM;
                line = "Waiting for your PC…";
                hint = "Open lagless on your PC. This phone shows up there as “" + deviceName + "”.";
        }
        dot.setTextColor(col);
        statusLine.setText(line);
        hintLine.setText(hint);
        hintLine.setVisibility(hint.isEmpty() ? View.GONE : View.VISIBLE);

        boolean live = phase.equals("playing") || phase.equals("idle");
        dLink.setText(live ? s.optString("link") + (s.optString("link_name").isEmpty() ? "" : " · " + s.optString("link_name")) : "–");
        dFormat.setText(live ? format(s) + (s.optBoolean("lossless") ? " · lossless" : "") : "–");
        dBuffer.setText(s.optDouble("target_ms", 0) > 0 ? String.format(Locale.ROOT, "%.1f ms (target %.1f)", s.optDouble("buffer_ms"), s.optDouble("target_ms")) : "–");
        dJitter.setText(s.optDouble("jitter_ms", 0) > 0 ? String.format(Locale.ROOT, "%.1f ms", s.optDouble("jitter_ms")) : "–");
        dDrops.setText(s.optInt("fixed") + " repaired · " + s.optInt("lost") + " lost · " + s.optInt("underruns") + " rebuffers");

        footer.setText("v" + s.optString("version"));
        String uv = s.optString("update_version");
        updateUrl = s.optString("update_url");
        updateButton.setVisibility(uv.isEmpty() ? View.GONE : View.VISIBLE);
        updateButton.setText("Get " + uv);
    }

    private static String format(JSONObject s) {
        String codec = s.optString("codec");
        int rate = s.optInt("rate");
        String khz = rate % 1000 == 0 ? String.valueOf(rate / 1000) : String.format(Locale.ROOT, "%.1f", rate / 1000.0);
        switch (codec) {
            case "pcm24": return "24-bit/" + khz + " kHz";
            case "pcm16": return "16-bit/" + khz + " kHz";
            case "opus": return "Opus";
            default: return codec.isEmpty() ? "" : codec;
        }
    }

    private void showError(String msg) {
        errorText.setText(msg);
        errorCard.setVisibility(View.VISIBLE);
    }

    // ------------------------------------------------------------------ layout

    private View buildUi() {
        ScrollView scroll = new ScrollView(this);
        scroll.setBackgroundColor(BG);
        scroll.setFillViewport(true);
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(dp(20), dp(24), dp(20), dp(24));
        scroll.addView(col);

        TextView title = text("lagless", 34, TEXT);
        title.setTypeface(Typeface.create("sans-serif", Typeface.BOLD));
        col.addView(title);
        TextView sub = text(deviceName, 15, DIM);
        sub.setPadding(0, dp(2), 0, dp(18));
        col.addView(sub);

        // Status card: the big latency number and one line of status.
        LinearLayout status = card();
        status.setGravity(Gravity.CENTER_HORIZONTAL);
        LinearLayout numRow = new LinearLayout(this);
        numRow.setGravity(Gravity.BOTTOM);
        latency = text("–", 72, TEXT);
        latency.setTypeface(Typeface.create("sans-serif-light", Typeface.NORMAL));
        unit = text(" ms", 22, DIM);
        unit.setPadding(0, 0, 0, dp(14));
        numRow.addView(latency);
        numRow.addView(unit);
        status.addView(numRow);
        LinearLayout lineRow = new LinearLayout(this);
        lineRow.setGravity(Gravity.CENTER);
        dot = text("● ", 12, DIM);
        statusLine = text("Starting…", 14, DIM);
        lineRow.addView(dot);
        lineRow.addView(statusLine);
        status.addView(lineRow);
        hintLine = text("", 13, DIM);
        hintLine.setGravity(Gravity.CENTER);
        hintLine.setPadding(0, dp(10), 0, 0);
        status.addView(hintLine);
        col.addView(status);

        // Volume card.
        col.addView(section("VOLUME"));
        LinearLayout vcard = card();
        LinearLayout vrow = new LinearLayout(this);
        vrow.setGravity(Gravity.CENTER_VERTICAL);
        volume = new SeekBar(this);
        volume.setMax(1000);
        volume.setProgress(Math.round(prefs().getFloat("volume", 1f) * 1000));
        volume.setProgressTintList(ColorStateList.valueOf(GREEN));
        volume.setThumbTintList(ColorStateList.valueOf(Color.WHITE));
        volume.setProgressBackgroundTintList(ColorStateList.valueOf(Color.rgb(58, 58, 60)));
        volumePct = text("", 14, DIM);
        volumePct.setMinWidth(dp(44));
        volumePct.setGravity(Gravity.END);
        vrow.addView(volume, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
        vrow.addView(volumePct);
        vcard.addView(vrow);
        col.addView(vcard);
        setPct(volume.getProgress());
        volume.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override public void onProgressChanged(SeekBar b, int p, boolean user) {
                setPct(p);
                if (libraryLoaded) Engine.setVolume(p / 1000f);
            }
            @Override public void onStartTrackingTouch(SeekBar b) {}
            @Override public void onStopTrackingTouch(SeekBar b) {
                prefs().edit().putFloat("volume", b.getProgress() / 1000f).apply();
            }
        });

        // Details card.
        col.addView(section("DETAILS"));
        LinearLayout dcard = card();
        dLink = row(dcard, "Link");
        dFormat = row(dcard, "Format");
        dBuffer = row(dcard, "Buffer");
        dJitter = row(dcard, "Arrival jitter");
        dDrops = row(dcard, "Dropouts");
        col.addView(dcard);

        // Errors (hidden until there is one).
        errorCard = card();
        errorCard.setVisibility(View.GONE);
        ((GradientDrawable) errorCard.getBackground()).setColor(Color.rgb(58, 24, 24));
        errorText = text("", 13, Color.rgb(255, 180, 175));
        errorText.setTextIsSelectable(true);
        errorCard.addView(errorText);
        LinearLayout.LayoutParams ep = (LinearLayout.LayoutParams) errorCard.getLayoutParams();
        ep.topMargin = dp(16);
        col.addView(errorCard);

        // Footer: version and update.
        LinearLayout foot = new LinearLayout(this);
        foot.setGravity(Gravity.CENTER_VERTICAL);
        foot.setPadding(dp(4), dp(18), dp(4), 0);
        footer = text("", 13, DIM);
        foot.addView(footer, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
        updateButton = new Button(this);
        updateButton.setAllCaps(false);
        updateButton.setTextColor(Color.WHITE);
        GradientDrawable ub = new GradientDrawable();
        ub.setColor(BLUE);
        ub.setCornerRadius(dp(18));
        updateButton.setBackground(ub);
        updateButton.setPadding(dp(16), 0, dp(16), 0);
        updateButton.setVisibility(View.GONE);
        updateButton.setOnClickListener(v -> {
            if (!updateUrl.isEmpty()) startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(updateUrl)));
        });
        foot.addView(updateButton);
        col.addView(foot);
        return scroll;
    }

    private void setPct(int p) {
        volumePct.setText(Math.round(p / 10f) + "%");
    }

    private LinearLayout card() {
        LinearLayout c = new LinearLayout(this);
        c.setOrientation(LinearLayout.VERTICAL);
        c.setPadding(dp(18), dp(16), dp(18), dp(16));
        GradientDrawable bg = new GradientDrawable();
        bg.setColor(CARD);
        bg.setCornerRadius(dp(16));
        c.setBackground(bg);
        c.setLayoutParams(new LinearLayout.LayoutParams(LinearLayout.LayoutParams.MATCH_PARENT, LinearLayout.LayoutParams.WRAP_CONTENT));
        return c;
    }

    private TextView section(String s) {
        TextView t = text(s, 13, DIM);
        t.setPadding(dp(16), dp(22), 0, dp(8));
        return t;
    }

    private TextView row(LinearLayout parent, String key) {
        LinearLayout r = new LinearLayout(this);
        r.setPadding(0, dp(6), 0, dp(6));
        TextView k = text(key, 15, DIM);
        TextView v = text("–", 15, TEXT);
        v.setGravity(Gravity.END);
        r.addView(k, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1));
        r.addView(v, new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 2));
        parent.addView(r);
        return v;
    }

    private TextView text(String s, float sp, int color) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setTextColor(color);
        return t;
    }

    private int dp(float v) {
        return Math.round(v * getResources().getDisplayMetrics().density);
    }

    private SharedPreferences prefs() {
        return getSharedPreferences("lagless", MODE_PRIVATE);
    }

    /** The name the user gave the phone ("Galaxy S24 FE"), else the model. */
    private String deviceName() {
        String n = null;
        try {
            n = Settings.Global.getString(getContentResolver(), "device_name");
        } catch (RuntimeException ignored) {
        }
        return n == null || n.trim().isEmpty() ? Build.MODEL : n;
    }

    /** Any uncaught Java error opens the crash screen (its own process) with the details. */
    private void installCrashScreen() {
        Thread.setDefaultUncaughtExceptionHandler((thread, e) -> {
            StringWriter sw = new StringWriter();
            e.printStackTrace(new PrintWriter(sw));
            Intent i = new Intent(getApplicationContext(), CrashActivity.class)
                    .putExtra("text", "lagless on " + Build.MODEL + " (Android " + Build.VERSION.RELEASE + ")\n\n" + sw)
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK);
            startActivity(i);
            android.os.Process.killProcess(android.os.Process.myPid());
        });
    }
}
