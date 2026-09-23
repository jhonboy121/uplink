package dev.uplink;

import android.app.NativeActivity;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.PictureInPictureParams;
import android.app.RemoteAction;
import android.content.BroadcastReceiver;
import android.content.ClipData;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.res.Configuration;
import android.graphics.drawable.Icon;
import android.os.Build;
import android.os.Bundle;
import android.util.Rational;

import java.util.ArrayList;
import java.util.List;
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
    /** The picture-in-picture window's shape: upright, like the phones on both ends of a call. */
    private static final int PIP_WIDTH = 9;
    private static final int PIP_HEIGHT = 16;
    /** Our own broadcast, sent only to ourselves; the codes match `platform::CallAction` in Rust. */
    private static final String ACTION_CALL = "dev.uplink.CALL_ACTION";
    private static final String EXTRA_ACTION = "action";
    private static final int ACTION_HANGUP = 0;
    private static final int ACTION_MIC = 1;

    /**
     * Static so {@link #log} works from anywhere in the app (the call service has no activity).
     * There is one activity per process; it is cleared before the handle is released.
     */
    private static volatile long nativeHandle;

    /** What the UI last asked the system bars to look like; see {@link #applySystemBars()}. */
    private volatile boolean lightSystemBars;

    /** A call is running, so leaving the app should shrink it rather than hide it. */
    private volatile boolean inCall;
    /** Mirrors the UI, so the window's own mute button shows the state it would move to. */
    private volatile boolean micOn = true;

    /** Taps on the picture-in-picture buttons arrive here; nothing outside the app may send them. */
    private final BroadcastReceiver callActions = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            long handle = nativeHandle;
            if (handle != 0) {
                nativeCallAction(handle, intent.getIntExtra(EXTRA_ACTION, -1));
            }
        }
    };

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        IntentFilter filter = new IntentFilter(ACTION_CALL);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            // Required from 33, and right at any version: these are ours to send.
            registerReceiver(callActions, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(callActions, filter);
        }
    }


    private static native void nativePermissionsResult(
            long handle, int requestCode, String[] permissions, int[] grantResults);

    private static native void nativeLog(long handle, int priority, String message);

    private static native void nativeImagePicked(long handle, int requestCode, int[] pixels, int width, int height);

    private static native void nativeDetach(long handle);

    private static native void nativePictureInPicture(long handle, boolean active);

    private static native void nativeCallAction(long handle, int action);

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

    /** Told by the UI, so the window's mute button offers the move the user has not made yet. */
    void setMicOn(boolean on) {
        micOn = on;
        refreshPictureInPicture();
    }

    /**
     * Whether the app may post notifications. Before Android 13 this is the only answer there is,
     * since there is no permission to hold; from 13 on it agrees with the permission.
     */
    boolean notificationsEnabled() {
        NotificationManager notifications = getSystemService(NotificationManager.class);
        return notifications != null && notifications.areNotificationsEnabled();
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

    /**
     * Back always goes to Rust, which knows what is on screen; it answers by closing something or
     * by calling {@link #moveToBackground()}. Nothing here calls {@code super}, because finishing
     * the activity would take the endpoint with it and there would be nothing left to ring.
     */
    /** Leaves the app running with its task behind everything else, rather than tearing it down. */
    void moveToBackground() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                moveTaskToBack(true);
            }
        });
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applySystemBars();
        }
    }

    /**
     * Offers a file Rust has already written into {@link UplinkFiles#DIRECTORY} to the share
     * sheet. Nothing is composed here — the identity card is drawn in `uplink_core::card`, where
     * the one thing that matters about it, that it still scans, can be tested.
     */
    void shareFile(String name, final String title) throws IOException {
        final Uri uri = UplinkFiles.uriFor(this, name);
        final String type = UplinkFiles.typeOf(name);
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Intent send = new Intent(Intent.ACTION_SEND);
                send.setType(type);
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

    /**
     * Leaving the app during a call shrinks it to a picture-in-picture window rather than leaving
     * the call running behind a blank screen. `onUserLeaveHint` is the press of Home or a switch
     * to another app; a new activity opening over ours is not the user leaving and does not count.
     */
    @Override
    protected void onUserLeaveHint() {
        super.onUserLeaveHint();
        log(Log.INFO, "user leaving, call " + inCall + ", already small " + isInPictureInPictureMode());
        if (inCall && !isInPictureInPictureMode()) {
            try {
                enterPictureInPictureMode(pictureInPictureParams());
            } catch (RuntimeException e) {
                log(Log.WARN, "entering picture-in-picture: " + e);
            }
        }
    }

    @Override
    public void onPictureInPictureModeChanged(boolean active, Configuration config) {
        super.onPictureInPictureModeChanged(active, config);
        log(Log.INFO, "picture-in-picture " + active);
        long handle = nativeHandle;
        if (handle != 0) {
            nativePictureInPicture(handle, active);
        }
    }

    /**
     * Portrait, because both ends of a call are phones held upright and the picture is cropped to
     * fill either way. The actions are what the window can be driven with once it is small.
     */
    private PictureInPictureParams pictureInPictureParams() {
        PictureInPictureParams.Builder params = new PictureInPictureParams.Builder()
                .setAspectRatio(new Rational(PIP_WIDTH, PIP_HEIGHT))
                .setActions(callActions());
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Hands the shrink animation to the system, so the home gesture is one movement
            // instead of the window appearing after it.
            params.setAutoEnterEnabled(true);
        }
        return params.build();
    }

    /**
     * Arms the system's own shrink and keeps the window's buttons current. Published when a call
     * starts, not only when the user leaves: `setAutoEnterEnabled` only takes effect if the
     * params were registered beforehand, and with gesture navigation `onUserLeaveHint` is not
     * dependable — which is the reason auto-enter exists at all.
     */
    void refreshPictureInPicture() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                try {
                    setPictureInPictureParams(inCall ? pictureInPictureParams() : idleParams());
                } catch (RuntimeException e) {
                    log(Log.WARN, "picture-in-picture params: " + e);
                }
            }
        });
    }

    /**
     * A call hung up from the small window's own button leaves the window with nothing in it, so
     * the window goes. Android offers no way out of picture-in-picture but finishing or coming
     * back to the front, and finishing would take the endpoint with it — so the task steps behind
     * everything instead, which dismisses the window and leaves the app running.
     */
    private void leavePictureInPicture() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (isInPictureInPictureMode()) {
                    moveTaskToBack(true);
                }
            }
        });
    }

    /** Nothing to watch, so leaving the app should just leave it. */
    private PictureInPictureParams idleParams() {
        PictureInPictureParams.Builder params = new PictureInPictureParams.Builder();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            params.setAutoEnterEnabled(false);
        }
        return params.build();
    }

    private List<RemoteAction> callActions() {
        List<RemoteAction> actions = new ArrayList<>();
        actions.add(action(micOn ? "mic" : "mic_off", micOn ? "Mute" : "Unmute", ACTION_MIC));
        actions.add(action("call_end", "End call", ACTION_HANGUP));
        return actions;
    }

    private RemoteAction action(String drawable, String title, int code) {
        int id = getResources().getIdentifier(drawable, "drawable", getPackageName());
        Intent intent = new Intent(ACTION_CALL).setPackage(getPackageName()).putExtra(EXTRA_ACTION, code);
        PendingIntent pending = PendingIntent.getBroadcast(
                this, code, intent, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        return new RemoteAction(Icon.createWithResource(this, id), title, title, pending);
    }

    /** Starts or stops the foreground service that keeps a call alive in the background. */
    void setCallService(boolean running) {
        inCall = running;
        // Arms the shrink now, while the app is still in front; too late once the user has left.
        refreshPictureInPicture();
        if (!running) {
            leavePictureInPicture();
        }
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
        unregisterReceiver(callActions);
        long handle = nativeHandle;
        nativeHandle = 0;
        if (handle != 0) {
            nativeDetach(handle);
        }
        super.onDestroy();
    }
}
