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
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.content.pm.ResolveInfo;
import android.graphics.drawable.Icon;
import android.media.AudioAttributes;
import android.media.AudioManager;
import android.media.Ringtone;
import android.media.RingtoneManager;
import android.media.ToneGenerator;
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

import java.io.IOException;

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

    private volatile boolean inCall;
    private volatile String peer = "";
    private volatile boolean micOn = true;

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

    private native long nativeStart(String dataDir);

    private static native void nativeLog(long core, int priority, String message);

    private static native void nativeRingAction(long core, int action);

    @Override
    public void onCreate() {
        super.onCreate();
        // Loaded here rather than left to NativeActivity, because a boot or a restarted service
        // has no activity to load it — and it is loading through Java that runs `JNI_OnLoad`,
        // which is what registers the natives above. NativeActivity later finds it loaded.
        try {
            System.loadLibrary(libraryName());
        } catch (PackageManager.NameNotFoundException | UnsatisfiedLinkError e) {
            Log.e(TAG, "loading the native library: " + e);
        }
        registerActivityLifecycleCallbacks(visibility);
        IntentFilter filter = new IntentFilter(ACTION_RING);
        filter.addAction(ACTION_MISSED_DISMISSED);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            registerReceiver(ringActions, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(ringActions, filter);
        }
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
        Intent settings = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, packageUri());
        settings.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        startActivity(settings);
    }

    Uri packageUri() {
        return Uri.fromParts("package", getPackageName(), null);
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
     * app is certainly in front; a later call for the same call only renames its notification.
     */
    void setCall(boolean running, String who) {
        boolean already = inCall;
        inCall = running;
        peer = who;
        if (!running) {
            stopService(callIntent());
        } else if (already) {
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
                .putExtra(UplinkCallService.EXTRA_MIC_ON, micOn);
    }

    // ---- Ringing -------------------------------------------------------------------------------

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
                        MISSED_CHANNEL, "Missed calls", NotificationManager.IMPORTANCE_DEFAULT);
                notifications.createNotificationChannel(channel);
                Intent open = new Intent(UplinkApplication.this, UplinkActivity.class)
                        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
                PendingIntent tap = PendingIntent.getActivity(UplinkApplication.this, REQUEST_MISSED, open,
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
                Intent dismissing = new Intent(ACTION_MISSED_DISMISSED).setPackage(getPackageName());
                PendingIntent dismissed = PendingIntent.getBroadcast(UplinkApplication.this, REQUEST_MISSED,
                        dismissing, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
                Notification notification = new Notification.Builder(UplinkApplication.this, MISSED_CHANNEL)
                        .setSmallIcon(drawable("notification"))
                        .setContentTitle(missed == 1 ? "Missed call" : missed + " missed calls")
                        .setContentText(missed == 1 ? who : "Latest from " + who)
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
                new NotificationChannel(RING_CHANNEL, "Incoming calls", NotificationManager.IMPORTANCE_HIGH);
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
                .setSmallIcon(drawable("notification"))
                .setContentTitle(who)
                .setContentText("Incoming video call")
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
                            Icon.createWithResource(this, drawable("call_end")), "Decline", decline).build())
                    .addAction(new Notification.Action.Builder(
                            Icon.createWithResource(this, drawable("notification")), "Answer", answer).build());
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

    /** By name: the resource table is ours, and there is no generated R class to look in. */
    private int drawable(String name) {
        return getResources().getIdentifier(name, "drawable", getPackageName());
    }
}
