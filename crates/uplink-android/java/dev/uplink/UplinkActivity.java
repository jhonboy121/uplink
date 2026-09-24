package dev.uplink;

import android.app.NativeActivity;
import android.app.PendingIntent;
import android.app.PictureInPictureParams;
import android.app.RemoteAction;
import android.content.ActivityNotFoundException;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.res.Configuration;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.drawable.Icon;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.util.Log;
import android.util.Rational;
import android.view.WindowInsetsController;

import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;

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
    /**
     * Our own broadcast, sent only to ourselves; the codes match `PlatformEvent::action` in Rust.
     * Shared with {@link UplinkCallService}, whose notification carries the same two buttons, so
     * a tap means the same thing wherever it came from.
     */
    static final String ACTION_CALL = "dev.uplink.CALL_ACTION";
    static final String EXTRA_ACTION = "action";
    static final int ACTION_HANGUP = 0;
    static final int ACTION_MIC = 1;
    static final int ACTION_ANSWER = 2;
    /** Why the activity was opened for a call, from the ringing notification. */
    static final String EXTRA_CALL = "call";
    static final int CALL_SHOW = 1;
    static final int CALL_ANSWER = 2;

    /** Cleared before the handle is released. */
    private volatile long nativeHandle;

    /**
     * Answer was tapped before the window was up to take it. Answering is the window's to do,
     * because the call's media goes to whoever is attached when it connects.
     */
    private volatile boolean answerPending;

    /** Request codes are Rust's and never negative, so this marks none outstanding. */
    private static final int NO_REQUEST = -1;
    /**
     * The request code of the battery dialog, if one is open, so its result is not mistaken for
     * an image pick. Main thread only, like the results themselves.
     */
    private int batteryRequest = NO_REQUEST;

    /** What the UI last asked the system bars to look like; see {@link #applySystemBars()}. */
    private volatile boolean lightSystemBars;

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
        takeCallIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        takeCallIntent(intent);
    }

    /**
     * Opened from a ringing call: show over the lock screen and wake it, which is what a phone
     * does with a call. Undone when the call is over, in {@link #setInCall}, so the app is not
     * left readable to anyone holding a locked phone.
     */
    private void takeCallIntent(Intent intent) {
        int call = intent != null ? intent.getIntExtra(EXTRA_CALL, 0) : 0;
        if (call == 0) {
            return;
        }
        // Consumed, so a later recreation does not answer a call that is long gone.
        intent.removeExtra(EXTRA_CALL);
        setShowWhenLocked(true);
        setTurnScreenOn(true);
        if (call == CALL_ANSWER) {
            answerPending = true;
            deliverAnswer();
        }
    }

    private void deliverAnswer() {
        long handle = nativeHandle;
        if (handle != 0 && answerPending) {
            answerPending = false;
            nativeCallAction(handle, ACTION_ANSWER);
        }
    }

    private static native void nativePermissionsResult(
            long handle, int requestCode, String[] permissions, int[] grantResults);

    private static native void nativeImagePicked(long handle, int requestCode, int[] pixels, int width, int height);

    private static native void nativeDetach(long handle);

    private static native void nativePictureInPicture(long handle, boolean active);

    private static native void nativeCallAction(long handle, int action);

    /** A screen started for a result has closed; the caller reads the outcome for itself. */
    private static native void nativeActivityDone(long handle, int requestCode);

    /** Logs to logcat and, once the core is up, into the app's own log file. */
    static void log(int priority, String message) {
        UplinkApplication.log(priority, message);
    }

    /** Called from Rust once natives are registered. */
    void attachNative(long handle) {
        nativeHandle = handle;
        log(Log.DEBUG, "native bridge attached");
        deliverAnswer();
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

    /** The process-wide half of the platform, which outlives this activity. */
    private UplinkApplication app() {
        return (UplinkApplication) getApplication();
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
     * Android's one-tap dialog for the battery exemption, or its list of apps where there is no
     * dialog. Started from here, in our own task: the dialog belongs to Settings, so started with
     * NEW_TASK from the Application it joins Settings' task whenever one exists, and the whole
     * Settings page comes up behind it. For a result, so the caller can wait until the user has
     * answered; what they answered is read back from Android rather than from the result code.
     */
    void requestBatteryExemption(final int requestCode) {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                batteryRequest = requestCode;
                try {
                    startActivityForResult(new Intent(
                            Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, app().packageUri()), requestCode);
                } catch (ActivityNotFoundException e) {
                    log(Log.WARN, "no battery exemption dialog: " + e);
                    startActivityForResult(new Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS), requestCode);
                }
            }
        });
    }

    /**
     * Leaves the app running with its task behind everything else, rather than tearing it down —
     * a finished activity would take the endpoint with it and leave nothing to ring.
     */
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
        if (requestCode == batteryRequest) {
            batteryRequest = NO_REQUEST;
            nativeActivityDone(handle, requestCode);
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
        log(Log.INFO, "user leaving, call " + app().inCall() + ", already small " + isInPictureInPictureMode());
        if (app().inCall() && !isInPictureInPictureMode()) {
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
                    setPictureInPictureParams(app().inCall() ? pictureInPictureParams() : idleParams());
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
        boolean micOn = app().micOn();
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

    /**
     * A call started or ended. The service and the notification are the Application's; what is
     * left here is the window: arming the shrink while the app is still in front, and dismissing
     * it when there is nothing to watch.
     */
    void setInCall(boolean running) {
        refreshPictureInPicture();
        if (!running) {
            leavePictureInPicture();
            leaveLockScreen();
        }
    }

    private void leaveLockScreen() {
        runOnUiThread(new Runnable() {
            @Override
            public void run() {
                setShowWhenLocked(false);
                setTurnScreenOn(false);
            }
        });
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
