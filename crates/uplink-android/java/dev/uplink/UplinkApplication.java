package dev.uplink;

import android.app.Activity;
import android.app.Application;
import android.app.NativeActivity;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Person;
import android.content.ActivityNotFoundException;
import android.content.BroadcastReceiver;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.SharedPreferences;
import android.content.res.Configuration;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.drawable.Icon;
import android.hardware.camera2.CameraAccessException;
import android.hardware.camera2.CameraCharacteristics;
import android.hardware.camera2.CameraManager;
import android.hardware.camera2.params.StreamConfigurationMap;
import android.media.AudioAttributes;
import android.media.AudioManager;
import android.media.MediaCodec;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.media.MediaFormat;
import android.media.Ringtone;
import android.media.RingtoneManager;
import android.media.ToneGenerator;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.PowerManager;
import android.os.VibrationAttributes;
import android.os.VibrationEffect;
import android.os.Vibrator;
import android.provider.Settings;
import android.util.Log;
import android.util.Size;

import java.io.IOException;
import java.util.Arrays;
import java.util.Locale;

/**
 * Everything the app can do with only a `Context`, which is everything that still matters when
 * nobody is looking at the screen: starting the endpoint, ringing, reading whether notifications
 * are allowed, running the call service, handing a file to another app, opening settings pages.
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
    private static final String TAG = "uplink";
    private static final String RING_CHANNEL = "ring";
    private static final int RING_NOTIFICATION_ID = 3;
    private static final String MISSED_CHANNEL = "missed";
    private static final int MISSED_NOTIFICATION_ID = 4;
    private static final String UPDATE_CHANNEL = "update";
    private static final int UPDATE_NOTIFICATION_ID = 5;
    /** Our own broadcast for the ringing notification's Decline, sent only to ourselves. */
    private static final String ACTION_RING = "dev.uplink.RING_ACTION";
    /** The missed-call notification was swiped away or cleared. */
    private static final String ACTION_MISSED_DISMISSED = "dev.uplink.MISSED_DISMISSED";
    /** Must match `RingAction` in Rust. */
    private static final int RING_DECLINE = 0;
    /** On, off, on, off… for as long as it rings; the first zero starts it at once. */
    private static final long[] RING_VIBRATION = {0, 1000, 1000};
    private static final int RING_VIBRATION_REPEAT_FROM = 1;
    /** One request code per pending intent, so their extras never overwrite each other. */
    private static final int REQUEST_SHOW = 10;
    private static final int REQUEST_ANSWER = 11;
    private static final int REQUEST_DECLINE = 12;
    private static final int REQUEST_MISSED = 13;
    private static final int REQUEST_UPDATE = 14;
    /**
     * The app's language, kept here as well as in Rust's settings: a boot posts the listening
     * notification before the endpoint (and the database behind it) is up, and it should already
     * be in the language the user picked.
     */
    private static final String PREFERENCES = "uplink";
    private static final String LANGUAGE = "language";

    /**
     * Phone makers' own lists of apps they may stop in the background, beside Android's. None of
     * them has an API: these are the screens' component names as collected by the people who
     * document such things, they differ per maker and per version, and any of them can be missing
     * or not exported. So each is only offered when it resolves, and opening one never claims to
     * have changed anything. The first Huawei entry is the EMUI 9+ startup manager.
     */
    private static final ComponentName[] MAKER_LISTS = {
        new ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity"),
        new ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.optimize.process.ProtectActivity"),
        new ComponentName("com.huawei.systemmanager", "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity"),
        new ComponentName("com.miui.securitycenter", "com.miui.permcenter.autostart.AutoStartManagementActivity"),
        new ComponentName("com.coloros.safecenter", "com.coloros.safecenter.permission.startup.StartupAppListActivity"),
        new ComponentName("com.coloros.safecenter", "com.coloros.safecenter.startupapp.StartupAppListActivity"),
        new ComponentName("com.oppo.safe", "com.oppo.safe.permission.startup.StartupAppListActivity"),
        new ComponentName("com.vivo.permissionmanager", "com.vivo.permissionmanager.activity.BgStartUpManagerActivity"),
        new ComponentName("com.iqoo.secure", "com.iqoo.secure.ui.phoneoptimize.AddWhiteListActivity"),
        new ComponentName("com.oneplus.security", "com.oneplus.security.chainlaunch.view.ChainLaunchAppListActivity"),
        new ComponentName("com.asus.mobilemanager", "com.asus.mobilemanager.autostart.AutoStartActivity"),
    };

    /**
     * The Rust side's endpoint and runtime, as an opaque pointer. It belongs to the process, not
     * to any activity: the activity is destroyed whenever the user swipes the app away, and on a
     * reboot there has never been one, but a call still has to have somewhere to arrive.
     *
     * <p>There is one Application per process, so this field is the one process-wide slot — which
     * is the honest place for it, and keeps the Rust side free of globals. Static only so that
     * {@link #log} can reach it from anywhere; it is written once, by {@link #ensureCore}.
     */
    private static volatile long core;

    /**
     * Resources in the language the app is set to, which is not necessarily the phone's: every
     * word outside the window is read through this. Itself when following the phone.
     */
    private volatile Context words = this;
    private volatile boolean inCall;
    private volatile String peer = "";
    private volatile boolean micOn = true;
    /** An `UplinkTelecom.ROUTE_*`, or `OUTPUT_MUTE`. */
    private volatile int output = UplinkTelecom.ROUTE_SPEAKER;
    /** Whether the call uses the camera: a voice call holds the microphone type alone. */
    private volatile boolean camera;

    // Ringing state. Touched only on the main thread, which is also where the lifecycle
    // callbacks arrive, so the two can never disagree about what is showing.
    private final Handler main = new Handler(Looper.getMainLooper());
    private String ringing;
    private boolean inFront;
    private Ringtone ringtone;
    private Vibrator vibrator;
    private ToneGenerator ringbackTone;
    /** Missed calls since the app was last opened, for the one notification that counts them. */
    private int missed;
    /** Calls as Telecom sees them; made in onCreate, before anything can place or ring one. */
    private UplinkTelecom telecom;

    /** Hands this Application to Rust's `ndk_context`, before anything there can read it. */
    private native void nativeInit();

    private native long nativeStart(String dataDir);

    private static native void nativeLog(long core, int priority, String message);

    private static native void nativeRingAction(long core, int action);

    private static native void nativeNetwork(long core, boolean up);

    private static native void nativeTelecom(long core, int action);

    @Override
    public void onCreate() {
        super.onCreate();
        // Loaded here rather than left to NativeActivity, because a boot or a restarted service
        // has no activity to load it — and it is loading through Java that runs `JNI_OnLoad`,
        // which is what registers the natives above. NativeActivity later finds it loaded.
        try {
            System.loadLibrary(libraryName());
            nativeInit();
        } catch (PackageManager.NameNotFoundException | UnsatisfiedLinkError e) {
            Log.e(TAG, "loading the native library: " + e);
        }
        words = localized(preferences().getString(LANGUAGE, ""));
        telecom = new UplinkTelecom(this);
        registerActivityLifecycleCallbacks(visibility);
        IntentFilter filter = new IntentFilter(ACTION_RING);
        filter.addAction(ACTION_MISSED_DISMISSED);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(ringActions, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(ringActions, filter);
        }
        getSystemService(ConnectivityManager.class).registerDefaultNetworkCallback(network, main);
    }

    /** The manifest names the library once, for NativeActivity; read it rather than repeat it. */
    private String libraryName() throws PackageManager.NameNotFoundException {
        ActivityInfo info = getPackageManager().getActivityInfo(
                new ComponentName(this, UplinkActivity.class), PackageManager.GET_META_DATA);
        return info.metaData.getString(NativeActivity.META_DATA_LIB_NAME);
    }

    /**
     * The endpoint, started on first use by whoever needs it first — the window, the listening
     * service, or a boot. Synchronized because two of them can ask at once, and a second start
     * would be a second endpoint on the same key and a second writer on the same log file.
     * Blocks until the endpoint is bound, so never call it on the main thread. Zero if it failed.
     */
    synchronized long ensureCore() {
        if (core == 0) {
            core = nativeStart(getFilesDir().getPath());
            // The core assumes a network. With none, no callback is coming to say otherwise.
            if (getSystemService(ConnectivityManager.class).getActiveNetwork() == null) {
                tellNetwork(false);
            }
        }
        return core;
    }

    boolean hasCore() {
        return core != 0;
    }

    /** Logs to logcat and, once the core is up, into the app's own log file. */
    static void log(int priority, String message) {
        Log.println(priority, TAG, message);
        long handle = core;
        if (handle != 0) {
            nativeLog(handle, priority, message);
        }
    }

    // ---- Words ---------------------------------------------------------------------------------

    /**
     * The language to say things in, as a `values-` folder names it; empty follows the phone.
     * Rust calls this on every start and whenever the setting changes.
     */
    void setLanguage(String code) {
        preferences().edit().putString(LANGUAGE, code).apply();
        words = localized(code);
    }

    private SharedPreferences preferences() {
        return getSharedPreferences(PREFERENCES, MODE_PRIVATE);
    }

    private Context localized(String code) {
        if (code.isEmpty()) {
            return this;
        }
        Configuration configuration = new Configuration(getResources().getConfiguration());
        configuration.setLocale(Locale.forLanguageTag(code));
        return createConfigurationContext(configuration);
    }

    /** A string resource in the app's language, formatted with `args`. */
    String text(int id, Object... args) {
        return words.getString(id, args);
    }

    /**
     * The few words Rust needs itself — the card's caption, the share sheet's titles, the
     * clipboard's label — by their place here. Must match `Text` in Rust's platform module.
     */
    private static final int[] RUST_TEXTS = {
        R.string.card_caption, R.string.share_identity, R.string.share_diagnostics, R.string.copy_key_label,
    };

    String rustText(int which) {
        return text(RUST_TEXTS[which]);
    }

    /** A plural resource in the app's language: the form for `count`, formatted with `args`. */
    String plural(int id, int count, Object... args) {
        return words.getResources().getQuantityString(id, count, args);
    }

    boolean inCall() {
        return inCall;
    }

    String peer() {
        return peer;
    }

    boolean micOn() {
        return micOn;
    }

    /** Where the call's sound goes, for the small window's output button. */
    void setOutput(int kind) {
        output = kind;
    }

    int output() {
        return output;
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
        Intent settings = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, packageUri());
        settings.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(settings);
    }

    Uri packageUri() {
        return Uri.fromParts("package", getPackageName(), null);
    }

    /**
     * This build's version as people see it — the manifest's versionName. It goes to the other
     * side of every call, so whichever phone is too old for the other can say so with both.
     */
    String appVersion() {
        try {
            String name = getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
            return name != null ? name : "";
        } catch (PackageManager.NameNotFoundException e) {
            log(Log.WARN, "reading our own version: " + e);
            return "";
        }
    }

    /**
     * A call that could not happen because one of the two phones needs a newer uplink: `name`'s,
     * or ours when `oursBehind`. Shown only while the app is not in front; in front, the app says
     * it on screen. A version the other side did not send is empty.
     */
    void updateNeeded(final boolean oursBehind, final String name, String theirsVersion, String oursVersion) {
        final String theirs = theirsVersion.isEmpty() ? text(R.string.unknown_version) : theirsVersion;
        final String ours = oursVersion.isEmpty() ? text(R.string.unknown_version) : oursVersion;
        main.post(new Runnable() {
            @Override
            public void run() {
                if (inFront) {
                    return;
                }
                String title = oursBehind ? text(R.string.update_ours_title) : text(R.string.update_theirs_title, name);
                String text = oursBehind
                        ? text(R.string.update_ours_text, name, theirs, ours)
                        : text(R.string.update_theirs_text, theirs, ours);
                NotificationManager notifications = getSystemService(NotificationManager.class);
                notifications.createNotificationChannel(new NotificationChannel(
                        UPDATE_CHANNEL, text(R.string.channel_update), NotificationManager.IMPORTANCE_DEFAULT));
                Intent open = new Intent(UplinkApplication.this, UplinkActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                PendingIntent tap = PendingIntent.getActivity(UplinkApplication.this, REQUEST_UPDATE, open,
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
                Notification notification = new Notification.Builder(UplinkApplication.this, UPDATE_CHANNEL)
                        .setSmallIcon(R.drawable.notification)
                        .setContentTitle(title)
                        .setContentText(text)
                        .setStyle(new Notification.BigTextStyle().bigText(text))
                        .setContentIntent(tap)
                        .setAutoCancel(true)
                        .build();
                notifications.notify(UPDATE_NOTIFICATION_ID, notification);
            }
        });
    }

    /**
     * Puts text on the clipboard. A key is public, so it is not marked sensitive: Android may
     * preview it in its copy confirmation, which is what the user wants to see.
     */
    void copyText(final String label, final String text) {
        main.post(new Runnable() {
            @Override
            public void run() {
                ClipboardManager clipboard = getSystemService(ClipboardManager.class);
                if (clipboard != null) {
                    clipboard.setPrimaryClip(ClipData.newPlainText(label, text));
                }
            }
        });
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

    /**
     * Keeps the process alive so the endpoint stays bound. Started once the endpoint is actually
     * up, so the notification never claims readiness the app does not have.
     */
    void setListening(boolean listening) {
        Intent intent = new Intent(this, UplinkListenService.class);
        if (listening) {
            startForegroundService(intent);
        } else {
            stopService(intent);
        }
    }

    /**
     * Runs the foreground service that lets a call keep the camera and microphone in the
     * background. Started once, on the tap that places or answers the call, which is when the
     * app is certainly in front; a later call for the same call only renames its notification,
     * unless it now uses the camera, which the service has to be started again to take.
     */
    void setCall(boolean running, String who, boolean withCamera) {
        boolean already = inCall;
        boolean gainsCamera = withCamera && !camera;
        inCall = running;
        peer = who;
        camera = running && withCamera;
        if (!running) {
            stopService(callIntent());
        } else if (already && !gainsCamera) {
            UplinkCallService.post(this, peer, micOn);
        } else {
            startForegroundService(callIntent());
        }
    }

    /** Re-posts the notification under the same id, so its mute button is never stale. */
    void setMicOn(boolean on) {
        micOn = on;
        if (inCall) {
            UplinkCallService.post(this, peer, micOn);
        }
    }

    private Intent callIntent() {
        return new Intent(this, UplinkCallService.class)
                .putExtra(UplinkCallService.EXTRA_PEER, peer)
                .putExtra(UplinkCallService.EXTRA_MIC_ON, micOn)
                .putExtra(UplinkCallService.EXTRA_CAMERA, camera);
    }

    // ---- Telecom -------------------------------------------------------------------------------

    UplinkTelecom telecom() {
        return telecom;
    }

    /** Answer, reject, hang up, or refused: what Telecom did with the call, for the core. */
    static void telecomAction(int action) {
        long handle = core;
        if (handle != 0) {
            nativeTelecom(handle, action);
        }
    }

    // What Rust calls, on any thread.

    boolean telecomPermitted(boolean incoming) {
        return telecom.permitted(incoming);
    }

    void telecomPlace(String key, String who, boolean video) {
        telecom.place(key, who, video);
    }

    void telecomIncoming(String key, String who, boolean video) {
        telecom.incoming(key, who, video);
    }

    void telecomActive() {
        telecom.active();
    }

    void telecomVideo(boolean on) {
        telecom.setVideo(on);
    }

    void telecomEnded(int cause) {
        telecom.ended(cause);
    }

    void telecomChooseRoute(int index) {
        telecom.route(index);
    }

    boolean telecomHeld() {
        return telecom.held();
    }

    boolean telecomMuted() {
        return telecom.muted();
    }

    int[] telecomRouteKinds() {
        return telecom.routeKinds();
    }

    String[] telecomRouteNames() {
        return telecom.routeNames();
    }

    int telecomCurrentRoute() {
        return telecom.route();
    }

    /**
     * Hold, mute or the outputs changed. The window reads them back when told; with no window
     * there is no media to hold, and one that opens later reads them when it starts.
     */
    void callAudioChanged() {
        sendBroadcast(new Intent(UplinkActivity.ACTION_CALL)
                .setPackage(getPackageName())
                .putExtra(UplinkActivity.EXTRA_ACTION, UplinkActivity.ACTION_AUDIO));
    }

    // ---- Ringing -------------------------------------------------------------------------------

    /** The volume key while it rings: the sound and vibration stop, the call still rings. */
    void silenceRinging() {
        main.post(new Runnable() {
            @Override
            public void run() {
                stopAlerting();
            }
        });
    }

    /**
     * Someone is calling. The sound plays whether or not the app is in front — the call screen
     * has none of its own — but the notification only shows while it is not, since in front the
     * call screen already says everything the notification would. Callable from any thread.
     */
    void ring(final String who) {
        main.post(new Runnable() {
            @Override
            public void run() {
                boolean already = ringing != null;
                ringing = who;
                if (!already) {
                    startAlerting();
                }
                showRing();
            }
        });
    }

    /**
     * The other phone is ringing. The platform's own ringback tone, on the media stream: before a
     * call connects the phone is not in call mode, so the voice-call stream would come out of the
     * earpiece of a phone held at arm's length for video. Callable from any thread.
     */
    void ringback() {
        main.post(new Runnable() {
            @Override
            public void run() {
                if (ringbackTone != null) {
                    return;
                }
                try {
                    ringbackTone = new ToneGenerator(AudioManager.STREAM_MUSIC, ToneGenerator.MAX_VOLUME);
                    ringbackTone.startTone(ToneGenerator.TONE_SUP_RINGTONE);
                } catch (RuntimeException e) {
                    // No tone is a quieter call, not a broken one.
                    log(Log.WARN, "ringback: " + e);
                    ringbackTone = null;
                }
            }
        });
    }

    /** Answered, declined, timed out or hung up: whichever it was, and whichever way, stop. */
    void stopRinging() {
        main.post(new Runnable() {
            @Override
            public void run() {
                if (ringbackTone != null) {
                    ringbackTone.stopTone();
                    ringbackTone.release();
                    ringbackTone = null;
                }
                if (ringing == null) {
                    return;
                }
                ringing = null;
                stopAlerting();
                showRing();
            }
        });
    }

    /**
     * A call rang and nobody answered. Shown only while uplink is not in front — in front, the
     * call screen was the notice — and folded into one notification that counts, cleared the
     * next time the app is opened.
     */
    void missedCall(final String who) {
        main.post(new Runnable() {
            @Override
            public void run() {
                if (inFront) {
                    return;
                }
                missed++;
                NotificationManager notifications = getSystemService(NotificationManager.class);
                NotificationChannel channel = new NotificationChannel(
                        MISSED_CHANNEL, text(R.string.channel_missed), NotificationManager.IMPORTANCE_DEFAULT);
                notifications.createNotificationChannel(channel);
                Intent open = new Intent(UplinkApplication.this, UplinkActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                PendingIntent tap = PendingIntent.getActivity(UplinkApplication.this, REQUEST_MISSED, open,
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
                Intent dismissing = new Intent(ACTION_MISSED_DISMISSED).setPackage(getPackageName());
                PendingIntent dismissed = PendingIntent.getBroadcast(UplinkApplication.this, REQUEST_MISSED,
                        dismissing, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
                Notification notification = new Notification.Builder(UplinkApplication.this, MISSED_CHANNEL)
                        .setSmallIcon(R.drawable.notification)
                        .setContentTitle(plural(R.plurals.missed_calls, missed, missed))
                        .setContentText(missed == 1 ? who : text(R.string.missed_latest, who))
                        .setCategory(Notification.CATEGORY_MISSED_CALL)
                        .setContentIntent(tap)
                        .setDeleteIntent(dismissed)
                        .setAutoCancel(true)
                        .build();
                notifications.notify(MISSED_NOTIFICATION_ID, notification);
            }
        });
    }

    /** Posts or withdraws the ringing notification to match the state. Main thread only. */
    private void showRing() {
        NotificationManager notifications = getSystemService(NotificationManager.class);
        if (ringing == null || inFront) {
            notifications.cancel(RING_NOTIFICATION_ID);
            return;
        }
        createRingChannel(notifications);
        try {
            notifications.notify(RING_NOTIFICATION_ID, ringNotification(ringing));
        } catch (RuntimeException e) {
            // A call-style notification the system refuses is still a call that must ring.
            log(Log.WARN, "incoming call notification: " + e);
        }
    }

    /**
     * Silent, because the sound is ours: a channel's sound plays once per notification, and a
     * call rings until someone does something about it. High importance is what makes it pop up
     * over whatever the user is doing.
     */
    private void createRingChannel(NotificationManager notifications) {
        NotificationChannel channel =
                new NotificationChannel(RING_CHANNEL, text(R.string.channel_ring), NotificationManager.IMPORTANCE_HIGH);
        channel.setSound(null, null);
        channel.enableVibration(false);
        channel.setLockscreenVisibility(Notification.VISIBILITY_PUBLIC);
        notifications.createNotificationChannel(channel);
    }

    private Notification ringNotification(String who) {
        PendingIntent show = activityIntent(REQUEST_SHOW, UplinkActivity.CALL_SHOW);
        PendingIntent answer = activityIntent(REQUEST_ANSWER, UplinkActivity.CALL_ANSWER);
        Intent declining = new Intent(ACTION_RING).setPackage(getPackageName());
        PendingIntent decline = PendingIntent.getBroadcast(
                this, REQUEST_DECLINE, declining, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification.Builder builder = new Notification.Builder(this, RING_CHANNEL)
                .setSmallIcon(R.drawable.notification)
                .setContentTitle(who)
                .setContentText(text(R.string.incoming_video_call))
                .setCategory(Notification.CATEGORY_CALL)
                .setVisibility(Notification.VISIBILITY_PUBLIC)
                .setContentIntent(show)
                // Over the lock screen, or straight to the call screen when the phone is asleep;
                // a heads-up when it is in use.
                .setFullScreenIntent(show, true)
                .setOngoing(true);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            Person caller = new Person.Builder().setName(who).setImportant(true).build();
            builder.setStyle(Notification.CallStyle.forIncomingCall(caller, decline, answer));
        } else {
            builder.addAction(new Notification.Action.Builder(
                            Icon.createWithResource(this, R.drawable.call_end), text(R.string.decline), decline).build())
                    .addAction(new Notification.Action.Builder(
                            Icon.createWithResource(this, R.drawable.notification), text(R.string.answer), answer).build());
        }
        return builder.build();
    }

    private PendingIntent activityIntent(int request, int call) {
        Intent intent = new Intent(this, UplinkActivity.class)
                .putExtra(UplinkActivity.EXTRA_CALL, call)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        return PendingIntent.getActivity(
                this, request, intent, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    /**
     * The user's own ringtone and vibration, as their ringer mode allows: sound in normal mode,
     * vibration unless silent. Main thread only.
     */
    private void startAlerting() {
        AudioManager audio = getSystemService(AudioManager.class);
        int mode = audio != null ? audio.getRingerMode() : AudioManager.RINGER_MODE_NORMAL;
        AudioAttributes attributes = new AudioAttributes.Builder()
                .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
                .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                .build();
        if (mode == AudioManager.RINGER_MODE_NORMAL) {
            ringtone = RingtoneManager.getRingtone(this, RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE));
            if (ringtone != null) {
                ringtone.setAudioAttributes(attributes);
                ringtone.setLooping(true);
                ringtone.play();
            }
        }
        if (mode != AudioManager.RINGER_MODE_SILENT) {
            vibrator = getSystemService(Vibrator.class);
            if (vibrator != null && vibrator.hasVibrator()) {
                VibrationEffect pattern = VibrationEffect.createWaveform(RING_VIBRATION, RING_VIBRATION_REPEAT_FROM);
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    // Marked as a ringtone, so the system treats it by its settings for calls.
                    vibrator.vibrate(pattern, VibrationAttributes.createForUsage(VibrationAttributes.USAGE_RINGTONE));
                } else {
                    vibrator.vibrate(pattern);
                }
            }
        }
    }

    private void stopAlerting() {
        if (ringtone != null) {
            ringtone.stop();
            ringtone = null;
        }
        if (vibrator != null) {
            vibrator.cancel();
            vibrator = null;
        }
    }

    /**
     * Declining needs no window: it goes straight to the endpoint. The missed-call notification
     * being swiped away comes here too, so the next one counts from zero rather than from calls
     * the user has already dismissed. Both are ours alone; the receiver is not exported.
     */
    private final BroadcastReceiver ringActions = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            if (ACTION_MISSED_DISMISSED.equals(intent.getAction())) {
                missed = 0;
                return;
            }
            stopRinging();
            long handle = core;
            if (handle != 0) {
                nativeRingAction(handle, RING_DECLINE);
            }
        }
    };

    /**
     * iroh cannot watch the network on Android (netwatch's route monitor there is empty), so it is
     * told from here, for the whole process, window or not. Without this, a switch from wifi to
     * mobile data left DNS pointed at the wifi router, and a relay that dropped never came back.
     */
    private final ConnectivityManager.NetworkCallback network = new ConnectivityManager.NetworkCallback() {
        /** The default network's interface and DNS servers as last told, so repeats are skipped. */
        private String last = "";

        @Override
        public void onAvailable(Network network) {
            log(Log.INFO, "network: " + transport(network));
        }

        /**
         * For the chip only: the core does not pass this on to iroh. Told now, iroh would re-read
         * DNS while there is no default network, keep just the public fallbacks, and then count the
         * next network appearing as a minor change that does not re-read it. The next network's
         * link properties are the moment to tell it.
         */
        @Override
        public void onLost(Network network) {
            last = "";
            log(Log.INFO, "network: lost");
            tellNetwork(false);
        }

        @Override
        public void onLinkPropertiesChanged(Network network, LinkProperties link) {
            String now = link.getInterfaceName() + ", dns " + link.getDnsServers();
            if (now.equals(last)) {
                return;
            }
            last = now;
            log(Log.INFO, "network: " + now);
            tellNetwork(true);
        }
    };

    private String transport(Network network) {
        NetworkCapabilities capabilities = getSystemService(ConnectivityManager.class).getNetworkCapabilities(network);
        if (capabilities == null) {
            return "unknown";
        }
        if (capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
            return "wifi";
        }
        if (capabilities.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR)) {
            return "mobile";
        }
        return "other";
    }

    /** H.264: what every call's video is. */
    private static final String AVC = MediaFormat.MIMETYPE_VIDEO_AVC;
    private static final long NANOS_PER_SECOND = 1_000_000_000L;

    /**
     * Whether this phone can send a call's video at this size, rate and bitrate: the front camera
     * (where calls start) outputs that size to an encoder fast enough, and the encoder the app
     * gets for H.264 takes it. A step that fails either is not offered.
     */
    boolean canSend(int width, int height, int fps, int bitrate) {
        return cameraCanSend(width, height, fps) && encoderCanSend(width, height, fps, bitrate);
    }

    private boolean cameraCanSend(int width, int height, int fps) {
        CameraManager cameras = getSystemService(CameraManager.class);
        if (cameras == null) {
            return false;
        }
        try {
            for (String id : cameras.getCameraIdList()) {
                CameraCharacteristics camera = cameras.getCameraCharacteristics(id);
                Integer facing = camera.get(CameraCharacteristics.LENS_FACING);
                StreamConfigurationMap streams = camera.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP);
                if (facing == null || facing != CameraCharacteristics.LENS_FACING_FRONT || streams == null) {
                    continue;
                }
                Size[] sizes = streams.getOutputSizes(MediaCodec.class);
                for (Size size : sizes != null ? sizes : new Size[0]) {
                    if (size.getWidth() == width && size.getHeight() == height) {
                        // Stricter than the fps ranges: how often this size can come out at all.
                        long frame = streams.getOutputMinFrameDuration(MediaCodec.class, size);
                        return frame * fps <= NANOS_PER_SECOND;
                    }
                }
                return false;
            }
        } catch (CameraAccessException | RuntimeException e) {
            log(Log.WARN, "reading the camera's sizes: " + e);
        }
        return false;
    }

    /** The first H.264 encoder in the list: the one `AMediaCodec_createEncoderByType` gives us. */
    private static boolean encoderCanSend(int width, int height, int fps, int bitrate) {
        for (MediaCodecInfo codec : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
            if (!codec.isEncoder() || !Arrays.asList(codec.getSupportedTypes()).contains(AVC)) {
                continue;
            }
            MediaCodecInfo.VideoCapabilities video = codec.getCapabilitiesForType(AVC).getVideoCapabilities();
            return video != null
                    && video.areSizeAndRateSupported(width, height, fps)
                    && video.getBitrateRange().contains(bitrate);
        }
        return false;
    }

    /**
     * Whether the default network is Wi-Fi (or Ethernet): what decides a call's quality preset.
     * Anything else, or none, counts as mobile data, the one that costs.
     */
    boolean onWifi() {
        ConnectivityManager connectivity = getSystemService(ConnectivityManager.class);
        Network active = connectivity != null ? connectivity.getActiveNetwork() : null;
        NetworkCapabilities capabilities = active != null ? connectivity.getNetworkCapabilities(active) : null;
        return capabilities != null
                && (capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                        || capabilities.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET));
    }

    /** Before the core is up there is nothing to tell: {@link #ensureCore} says what is current. */
    private static void tellNetwork(boolean up) {
        long handle = core;
        if (handle != 0) {
            nativeNetwork(handle, up);
        }
    }

    /**
     * Whether any of our activities is in front, which decides whether a ringing call needs a
     * notification. Only the activity's own lifecycle can say; there is nothing to query.
     */
    private final ActivityLifecycleCallbacks visibility = new ActivityLifecycleCallbacks() {
        @Override
        public void onActivityResumed(Activity activity) {
            inFront = true;
            showRing();
            // Opening the app is where the Calls tab shows them; the count starts again.
            if (missed > 0) {
                missed = 0;
                getSystemService(NotificationManager.class).cancel(MISSED_NOTIFICATION_ID);
            }
        }

        @Override
        public void onActivityPaused(Activity activity) {
            inFront = false;
            showRing();
        }

        @Override
        public void onActivityCreated(Activity activity, Bundle state) {}

        @Override
        public void onActivityStarted(Activity activity) {}

        @Override
        public void onActivityStopped(Activity activity) {}

        @Override
        public void onActivitySaveInstanceState(Activity activity, Bundle state) {}

        @Override
        public void onActivityDestroyed(Activity activity) {}
    };

    // ---- Staying reachable ---------------------------------------------------------------------

    /** Whether Android's own battery optimisation leaves us alone. This one can be read. */
    boolean batteryUnrestricted() {
        PowerManager power = getSystemService(PowerManager.class);
        return power != null && power.isIgnoringBatteryOptimizations(getPackageName());
    }

    /**
     * Whether a call may take over the screen. From Android 14 the user can take that away, and
     * a call that cannot is only a heads-up — easy to miss with the phone in a pocket.
     */
    boolean fullScreenCallsAllowed() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            return true;
        }
        NotificationManager notifications = getSystemService(NotificationManager.class);
        return notifications != null && notifications.canUseFullScreenIntent();
    }

    void openFullScreenCallsSettings() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            openAppSettings();
            return;
        }
        try {
            startActivity(new Intent(Settings.ACTION_MANAGE_APP_USE_FULL_SCREEN_INTENT, packageUri())
                    .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
        } catch (ActivityNotFoundException e) {
            log(Log.WARN, "no full-screen intent settings: " + e);
            openAppSettings();
        }
    }

    /** The first maker's list this phone has and lets us open, or null. */
    private ComponentName makerList() {
        PackageManager packages = getPackageManager();
        for (ComponentName list : MAKER_LISTS) {
            ResolveInfo found = packages.resolveActivity(new Intent().setComponent(list), 0);
            if (found != null && found.activityInfo != null && found.activityInfo.exported) {
                return list;
            }
        }
        return null;
    }

    boolean hasMakerList() {
        return makerList() != null;
    }

    /**
     * Opens the maker's list, or our own settings page if it will not open after all. Nothing
     * here can tell whether the user then did anything, so nothing claims they did.
     */
    void openMakerList() {
        ComponentName list = makerList();
        if (list == null) {
            openAppSettings();
            return;
        }
        try {
            startActivity(new Intent().setComponent(list).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
        } catch (ActivityNotFoundException | SecurityException e) {
            log(Log.WARN, "opening " + list.flattenToShortString() + ": " + e);
            openAppSettings();
        }
    }
}
