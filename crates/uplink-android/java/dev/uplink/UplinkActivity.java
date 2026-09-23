package dev.uplink;

import android.content.ClipData;
import android.content.Intent;
import android.app.NativeActivity;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.net.Uri;
import android.provider.Settings;
import android.util.Log;
import android.view.WindowInsetsController;

import java.io.File;
import java.io.IOException;
import java.io.InputStream;

/**
 * NativeActivity plus the callbacks it drops. Rust hands over an opaque handle via
 * {@link #attachNative(long)}; every callback passes it back, and {@link #onDestroy()} releases it.
 */
public class UplinkActivity extends NativeActivity {
    private static final String TAG = "uplink";
    /** Longest edge kept when decoding a picked image; plenty for reading a QR code. */
    private static final int MAX_SCAN_PIXELS = 1600;

    /**
     * Static so {@link #log} works from anywhere in the app (the call service has no activity).
     * There is one activity per process; it is cleared before the handle is released.
     */
    private static volatile long nativeHandle;

    /** What the UI last asked the system bars to look like; see {@link #applySystemBars()}. */
    private volatile boolean lightSystemBars;

    private static native void nativePermissionsResult(
            long handle, int requestCode, String[] permissions, int[] grantResults);

    private static native void nativeLog(long handle, int priority, String message);

    private static native void nativeImagePicked(long handle, int requestCode, int[] pixels, int width, int height);

    private static native void nativeDetach(long handle);

    /** Logs to logcat and, when the bridge is up, into the app's own log file. */
    static void log(int priority, String message) {
        Log.println(priority, TAG, message);
        long handle = nativeHandle;
        if (handle != 0) {
            nativeLog(handle, priority, message);
        }
    }

    /** Called from Rust once natives are registered. */
    void attachNative(long handle) {
        nativeHandle = handle;
        log(Log.DEBUG, "native bridge attached");
    }

    /** Callable from any thread; the request runs on the UI thread. */
    void requestPermissionsAsync(final String[] permissions, final int requestCode) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                requestPermissions(permissions, requestCode);
            }
        });
    }

    /**
     * Whether Android would still show its own dialog for this permission. False after the user
     * has refused for good, which is the only case that needs sending them to Settings.
     */
    boolean shouldExplain(String permission) {
        return shouldShowRequestPermissionRationale(permission);
    }

    /** Opens this app's own page in Settings, not the top of Settings. */
    void openAppSettings() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent settings = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS);
                settings.setData(Uri.fromParts("package", getPackageName(), null));
                settings.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                startActivity(settings);
            }
        });
    }

    /**
     * The app draws under the system bars, so the bars have no background of their own: their
     * icons have to be told which way to go. `light` means a light surface behind them, which
     * Android answers with dark icons.
     */
    void setLightSystemBars(final boolean light) {
        lightSystemBars = light;
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                applySystemBars();
            }
        });
    }

    /**
     * There is no controller until the window has a view root, and another activity we came back
     * from may have left its own appearance behind, so the wanted state is kept and re-applied
     * rather than set once.
     */
    private void applySystemBars() {
        WindowInsetsController controller = getWindow().getInsetsController();
        if (controller == null) {
            return;
        }
        int bars = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
        controller.setSystemBarsAppearance(lightSystemBars ? bars : 0, bars);
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applySystemBars();
        }
    }

    /**
     * Offers a picture Rust has already written into {@link UplinkFiles#DIRECTORY} to the share
     * sheet. Nothing is drawn here: the card is composed in `uplink_core::card`, where the one
     * thing that matters about it — that it still scans — can be tested.
     */
    void shareImage(String name, final String title) throws IOException {
        final Uri uri = UplinkFiles.uriFor(this, name);
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent send = new Intent(Intent.ACTION_SEND);
                send.setType("image/png");
                send.putExtra(Intent.EXTRA_STREAM, uri);
                // The chooser reads the grant off the clip data, so a target that never looks at
                // EXTRA_STREAM still gets permission for the file.
                send.setClipData(ClipData.newUri(getContentResolver(), title, uri));
                send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
                startActivity(Intent.createChooser(send, title));
            }
        });
    }

    /** Opens the system picker; the chosen image comes back through {@link #onActivityResult}. */
    void pickImageAsync(final int requestCode) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
                intent.addCategory(Intent.CATEGORY_OPENABLE);
                intent.setType("image/*");
                startActivityForResult(intent, requestCode);
            }
        });
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        long handle = nativeHandle;
        if (handle == 0) {
            return;
        }
        Uri uri = resultCode == RESULT_OK && data != null ? data.getData() : null;
        if (uri == null) {
            nativeImagePicked(handle, requestCode, null, 0, 0);
            return;
        }
        try {
            // Two passes: measure, then decode subsampled — a camera photo is far bigger than a
            // QR code needs, and the full bitmap would be tens of megabytes.
            BitmapFactory.Options bounds = new BitmapFactory.Options();
            bounds.inJustDecodeBounds = true;
            InputStream measuring = getContentResolver().openInputStream(uri);
            BitmapFactory.decodeStream(measuring, null, bounds);
            measuring.close();

            BitmapFactory.Options options = new BitmapFactory.Options();
            options.inSampleSize = 1;
            int largest = Math.max(bounds.outWidth, bounds.outHeight);
            while (largest / options.inSampleSize > MAX_SCAN_PIXELS) {
                options.inSampleSize *= 2;
            }
            InputStream stream = getContentResolver().openInputStream(uri);
            Bitmap bitmap = BitmapFactory.decodeStream(stream, null, options);
            stream.close();
            if (bitmap == null) {
                log(Log.WARN, "could not read that image");
                nativeImagePicked(handle, requestCode, null, 0, 0);
                return;
            }
            int width = bitmap.getWidth(), height = bitmap.getHeight();
            int[] pixels = new int[width * height];
            bitmap.getPixels(pixels, 0, width, 0, 0, width, height);
            bitmap.recycle();
            nativeImagePicked(handle, requestCode, pixels, width, height);
        } catch (Exception e) {
            log(Log.WARN, "reading image: " + e);
            nativeImagePicked(handle, requestCode, null, 0, 0);
        }
    }

    /** Starts or stops the foreground service that keeps a call alive in the background. */
    void setCallService(boolean running) {
        Intent intent = new Intent(this, UplinkCallService.class);
        if (running) {
            startForegroundService(intent);
        } else {
            stopService(intent);
        }
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults);
        long handle = nativeHandle;
        if (handle == 0) {
            Log.w(TAG, "permission result " + requestCode + " without native bridge");
            return;
        }
        nativePermissionsResult(handle, requestCode, permissions, grantResults);
    }

    @Override
    protected void onDestroy() {
        long handle = nativeHandle;
        nativeHandle = 0;
        if (handle != 0) {
            nativeDetach(handle);
        }
        super.onDestroy();
    }
}
