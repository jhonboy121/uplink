package dev.uplink;

import android.content.Intent;
import android.app.NativeActivity;
import android.util.Log;

/**
 * NativeActivity plus the callbacks it drops. Rust hands over an opaque handle via
 * {@link #attachNative(long)}; every callback passes it back, and {@link #onDestroy()} releases it.
 */
public class UplinkActivity extends NativeActivity {
    private static final String TAG = "uplink";

    /**
     * Static so {@link #log} works from anywhere in the app (the call service has no activity).
     * There is one activity per process; it is cleared before the handle is released.
     */
    private static volatile long nativeHandle;

    private static native void nativePermissionsResult(
            long handle, int requestCode, String[] permissions, int[] grantResults);

    private static native void nativeLog(long handle, int priority, String message);

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
