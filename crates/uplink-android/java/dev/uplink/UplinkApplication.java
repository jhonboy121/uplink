package dev.uplink;

import android.app.Application;
import android.app.NotificationManager;
import android.content.ClipData;
import android.content.Intent;
import android.net.Uri;
import android.provider.Settings;

import java.io.IOException;

/**
 * Everything the app can do with only a `Context`, which is everything that still matters when
 * nobody is looking at the screen: reading whether notifications are allowed, running the call
 * service, handing a file to another app, opening our own settings page.
 *
 * <p>It lives on the Application rather than in static methods so Rust can call it the way it
 * calls the activity — on an object it already holds. A static method would mean finding the
 * class by name, and `FindClass` on a thread attached from native code searches the *system*
 * classloader, which has never heard of us.
 *
 * <p>Process-wide call state lives here too, because the notification and the shrunken window
 * both need it and there is exactly one call.
 */
public class UplinkApplication extends Application {
    /**
     * The Rust side's endpoint and runtime, as an opaque pointer. It belongs to the process, not
     * to any activity: the activity is destroyed whenever the user swipes the app away, and on a
     * reboot there has never been one, but a call still has to have somewhere to arrive.
     *
     * <p>There is one Application per process, so this field is the one process-wide slot — which
     * is the honest place for it, and keeps the Rust side free of globals.
     */
    private volatile long core;

    long core() {
        return core;
    }

    void setCore(long handle) {
        core = handle;
    }

    private volatile boolean inCall;
    private volatile String peer = "";
    private volatile boolean micOn = true;

    boolean inCall() {
        return inCall;
    }

    String peer() {
        return peer;
    }

    boolean micOn() {
        return micOn;
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
        Intent settings = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS);
        settings.setData(Uri.fromParts("package", getPackageName(), null));
        settings.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(settings);
    }

    /**
     * Offers a file Rust has already written into {@link UplinkFiles#DIRECTORY} to the share
     * sheet. Nothing is composed here — the identity card is drawn in `uplink_core::card`, where
     * the one thing that matters about it, that it still scans, can be tested.
     */
    void shareFile(String name, String title) throws IOException {
        Uri uri = UplinkFiles.uriFor(this, name);
        Intent send = new Intent(Intent.ACTION_SEND);
        send.setType(UplinkFiles.typeOf(name));
        send.putExtra(Intent.EXTRA_STREAM, uri);
        // The chooser reads the grant off the clip data, so a target that never looks at
        // EXTRA_STREAM still gets permission for the file.
        send.setClipData(ClipData.newUri(getContentResolver(), title, uri));
        send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        // A chooser started from something that is not an activity needs its own task.
        startActivity(Intent.createChooser(send, title).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
    }

    /** Runs the foreground service that lets a call keep the camera and microphone in the background. */
    void setCall(boolean running, String who) {
        inCall = running;
        peer = who;
        Intent intent = callIntent();
        if (running) {
            startForegroundService(intent);
        } else {
            stopService(intent);
        }
    }

    /** Re-posts the notification under the same id, so its mute button is never stale. */
    void setMicOn(boolean on) {
        micOn = on;
        if (inCall) {
            startForegroundService(callIntent());
        }
    }

    private Intent callIntent() {
        return new Intent(this, UplinkCallService.class)
                .putExtra(UplinkCallService.EXTRA_PEER, peer)
                .putExtra(UplinkCallService.EXTRA_MIC_ON, micOn);
    }
}
