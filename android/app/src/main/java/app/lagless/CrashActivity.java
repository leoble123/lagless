package app.lagless;

import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.graphics.Color;
import android.graphics.Typeface;
import android.os.Bundle;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

/**
 * Shown instead of silently closing when the native app fails to start, so the error can be
 * read, copied and reported. Started from Rust (see src/android.rs, show_crash).
 */
public class CrashActivity extends Activity {
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        String text = getIntent().getStringExtra("text");
        if (text == null) text = "(no details)";
        final String report = text;

        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(48, 96, 48, 48);
        root.setBackgroundColor(Color.rgb(15, 16, 19));

        TextView title = new TextView(this);
        title.setText("lagless couldn't start");
        title.setTextSize(22);
        title.setTextColor(Color.WHITE);
        root.addView(title);

        TextView hint = new TextView(this);
        hint.setText("Please send this to the developer (screenshot or Copy):");
        hint.setTextColor(Color.rgb(139, 141, 150));
        hint.setPadding(0, 24, 0, 24);
        root.addView(hint);

        Button copy = new Button(this);
        copy.setText("Copy");
        copy.setOnClickListener(v -> {
            ClipboardManager cm = getSystemService(ClipboardManager.class);
            cm.setPrimaryClip(ClipData.newPlainText("lagless error", report));
            copy.setText("Copied");
        });
        root.addView(copy);

        ScrollView scroll = new ScrollView(this);
        TextView body = new TextView(this);
        body.setText(report);
        body.setTextIsSelectable(true);
        body.setTypeface(Typeface.MONOSPACE);
        body.setTextSize(12);
        body.setTextColor(Color.rgb(232, 232, 234));
        body.setPadding(0, 24, 0, 0);
        scroll.addView(body);
        root.addView(scroll);

        setContentView(root);
    }
}
