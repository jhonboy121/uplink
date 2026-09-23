package dev.uplink;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Rect;
import android.graphics.RectF;
import android.graphics.Typeface;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;

/**
 * The picture of an identity: the key as a QR code, the uplink mark on a plate in its middle, and
 * a line underneath saying what to do with it.
 *
 * <p>Rust hands over the code's modules because nothing in the framework encodes one. Everything
 * else is drawn here, where there is a text engine and a PNG writer already.
 */
final class IdentityCard {
    /** Pixels per module. A framed code is a little over 50 modules, so this lands near 1100px. */
    private static final int MODULE = 21;
    /**
     * Paper around the code, in modules. Rust sends the code alone, so this is also its quiet
     * zone: the spec asks for four, and a decoder hunting for the finder patterns needs them.
     * The rest is so the card survives being cropped by whatever it is sent through.
     */
    private static final int MARGIN = 6;
    /** Between the code and the line under it, and under the line. */
    private static final int CAPTION_GAP = 2;
    private static final float CAPTION_SIZE = 3.4f;
    /**
     * The plate in the middle is round, so the mark inside it is the square on its diameter —
     * 1/√2 of the width. `Theme.mark-in-plate` says the same thing about the code on screen.
     */
    private static final float MARK_IN_PLATE = 0.707f;
    private static final String FONT = "Outfit-SemiBold.ttf";
    private static final String MARK = "mark-badge.png";
    private static final String FILE = "identity.png";
    /** The design's paper and ink; the code is drawn in ink like everything else. */
    private static final int PAPER = 0xFFFFFFFF;
    private static final int INK = 0xFF0E151E;
    private static final int QUALITY = 100;

    private IdentityCard() {}

    /**
     * Draws the card and writes it into the directory {@link UplinkFiles} serves, replacing
     * whatever was there. Returns the file.
     */
    static File write(Context context, byte[] modules, int size, int logo, String caption)
            throws IOException {
        if (size <= 0 || modules.length < size * size) {
            throw new IOException("a code of " + size + " needs " + size * size + " modules");
        }
        Bitmap card = draw(context, modules, size, logo, caption);
        File directory = new File(context.getCacheDir(), UplinkFiles.DIRECTORY);
        if (!directory.isDirectory() && !directory.mkdirs()) {
            throw new IOException("could not make " + directory);
        }
        File file = new File(directory, FILE);
        FileOutputStream out = new FileOutputStream(file);
        try {
            if (!card.compress(Bitmap.CompressFormat.PNG, QUALITY, out)) {
                throw new IOException("could not encode the card");
            }
        } finally {
            out.close();
            card.recycle();
        }
        return file;
    }

    private static Bitmap draw(Context context, byte[] modules, int size, int logo, String caption)
            throws IOException {
        int code = size * MODULE;
        int margin = MARGIN * MODULE;
        int width = code + margin * 2;

        Paint text = new Paint(Paint.ANTI_ALIAS_FLAG);
        text.setColor(INK);
        text.setTextSize(CAPTION_SIZE * MODULE);
        text.setTextAlign(Paint.Align.CENTER);
        text.setTypeface(typeface(context));
        // The caption is one line and the card is as wide as the code; a longer translation
        // shrinks to fit rather than running off the paper.
        float measured = text.measureText(caption);
        if (measured > code) {
            text.setTextSize(text.getTextSize() * code / measured);
        }
        Paint.FontMetrics metrics = text.getFontMetrics();
        int line = Math.round(metrics.descent - metrics.ascent);
        int height = margin + code + CAPTION_GAP * MODULE + line + margin;

        Bitmap card = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888);
        Canvas canvas = new Canvas(card);
        canvas.drawColor(PAPER);

        Paint ink = new Paint();
        ink.setColor(INK);
        for (int y = 0; y < size; y++) {
            for (int x = 0; x < size; x++) {
                if (modules[y * size + x] == 0) {
                    continue;
                }
                int left = margin + x * MODULE;
                int top = margin + y * MODULE;
                canvas.drawRect(left, top, left + MODULE, top + MODULE, ink);
            }
        }

        // The plate covers the middle of the code, which is why the code is drawn at the error
        // correction level that can lose it. `logo` is how many modules uplink-core says that is.
        float centreX = margin + code / 2f;
        float centreY = margin + code / 2f;
        float plate = logo * MODULE;
        Paint paper = new Paint(Paint.ANTI_ALIAS_FLAG);
        paper.setColor(PAPER);
        canvas.drawCircle(centreX, centreY, plate / 2f, paper);

        Bitmap mark = decode(context, MARK);
        float side = plate * MARK_IN_PLATE;
        RectF into = new RectF(centreX - side / 2f, centreY - side / 2f, centreX + side / 2f, centreY + side / 2f);
        Paint smooth = new Paint(Paint.ANTI_ALIAS_FLAG);
        smooth.setFilterBitmap(true);
        canvas.drawBitmap(mark, new Rect(0, 0, mark.getWidth(), mark.getHeight()), into, smooth);
        mark.recycle();

        canvas.drawText(caption, width / 2f, margin + code + CAPTION_GAP * MODULE - metrics.ascent, text);
        return card;
    }

    /** The app's own face, so the card reads as the app's rather than the device's. */
    private static Typeface typeface(Context context) {
        try {
            return Typeface.createFromAsset(context.getAssets(), FONT);
        } catch (RuntimeException e) {
            UplinkActivity.log(android.util.Log.WARN, "no " + FONT + ", falling back: " + e);
            return Typeface.create(Typeface.SANS_SERIF, Typeface.BOLD);
        }
    }

    private static Bitmap decode(Context context, String asset) throws IOException {
        InputStream stream = context.getAssets().open(asset);
        try {
            Bitmap bitmap = BitmapFactory.decodeStream(stream);
            if (bitmap == null) {
                throw new IOException("could not read " + asset);
            }
            return bitmap;
        } finally {
            stream.close();
        }
    }
}
