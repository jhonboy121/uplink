package dev.uplink;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Person;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps a call running while the app is in the background: Android only lets a foreground service
 * with the camera and microphone types keep using them once the activity is no longer visible.
 *
 * <p>Those two types can only be taken while the app is in front, so the service is started on
 * the tap that places or answers a call, not when the call connects — by then the user may have
 * gone to another app, and Android refuses the types with a SecurityException. Once running, the
 * notification is updated in place ({@link #post}) rather than by starting the service again.
 *
 * <p>Its notification is the call, as far as the rest of the system is concerned — from 31 in the
 * shape Android has for one, which is what puts a call in the status bar and hang-up in the shade
 * rather than a line of text you can only tap.
 */
public class UplinkCallService extends Service {
    private static final String CHANNEL = "call";
    private static final int NOTIFICATION_ID = 1;
    /** Who is on the other end, and what the mic is doing, so the buttons offer the right move. */
    static final String EXTRA_PEER = "peer";
    static final String EXTRA_MIC_ON = "mic";
    /**
     * The ground an ongoing call is painted on. Android colorizes a call notification with the
     * builder's colour and works out readable text from it — but only if it is given one. Left
     * unset it colorizes with COLOR_DEFAULT and the buttons come out the colour of the
     * background. This is the light theme's beacon, which white text sits on cleanly.
     */
    private static final int ACCENT = 0xFF0A6FC2;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String peer = intent != null ? intent.getStringExtra(EXTRA_PEER) : null;
        boolean micOn = intent == null || intent.getBooleanExtra(EXTRA_MIC_ON, true);
        Notification notification = notification(this, peer != null ? peer : "", micOn);
        try {
            startForeground(
                    NOTIFICATION_ID,
                    notification,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA | ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
            UplinkActivity.log(Log.INFO, "call service started");
        } catch (RuntimeException e) {
            // Refused because the app was not in front. The service still has to go foreground —
            // a started foreground service that never does is its own crash — so it takes the
            // one type that is allowed from here. The call carries on; the camera and microphone
            // go quiet until the app comes back.
            UplinkActivity.log(Log.WARN, "call service without camera and microphone: " + e);
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        }
        // The activity stops it when the call ends; no restart if Android kills us.
        return START_NOT_STICKY;
    }

    /**
     * Replaces the running call's notification, for a new name or a mute, without starting the
     * service again — which from the background would be refused all over again.
     */
    static void post(Context context, String peer, boolean micOn) {
        context.getSystemService(NotificationManager.class).notify(NOTIFICATION_ID, notification(context, peer, micOn));
    }

    /**
     * Default importance rather than low, because a call that is demoted shows as an ordinary
     * line instead of a call — but silent, since the call itself is the sound.
     */
    private static void createChannel(Context context) {
        NotificationChannel channel =
                new NotificationChannel(CHANNEL, "Calls", NotificationManager.IMPORTANCE_DEFAULT);
        channel.setSound(null, null);
        channel.enableVibration(false);
        context.getSystemService(NotificationManager.class).createNotificationChannel(channel);
    }

    private static Notification notification(Context context, String peer, boolean micOn) {
        createChannel(context);
        Intent open = new Intent(context, UplinkActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                context, 0, open, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification.Builder builder = new Notification.Builder(context, CHANNEL)
                // Our own mark, cropped to its bounds: the system draws a small icon flat from
                // its alpha, so it is a silhouette that should fill the box rather than sit in
                // a launcher icon's safe zone.
                .setSmallIcon(drawable(context, "notification"))
                .setContentIntent(tap)
                .setOngoing(true)
                .setColor(ACCENT)
                .setColorized(true);
        Notification.Action mute =
                action(context, micOn ? "mic" : "mic_off", micOn ? "Mute" : "Unmute", UplinkActivity.ACTION_MIC);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Names the person and owns the hang-up itself; the mute rides along beside it.
            Person who = new Person.Builder().setName(peer).setImportant(true).build();
            builder.setStyle(Notification.CallStyle.forOngoingCall(who, pending(context, UplinkActivity.ACTION_HANGUP)))
                    .addAction(mute)
                    // A call's controls are no use ten seconds after the call starts, which is
                    // how long the system may otherwise sit on a foreground service's notification.
                    .setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        } else {
            // loadLabel, not getString(labelRes): our manifest has a literal label, no resources.
            CharSequence title = peer.isEmpty()
                    ? context.getApplicationInfo().loadLabel(context.getPackageManager())
                    : peer;
            builder.setContentTitle(title)
                    .setContentText("Call in progress")
                    .addAction(action(context, "call_end", "End call", UplinkActivity.ACTION_HANGUP))
                    .addAction(mute);
        }
        return builder.build();
    }

    private static Notification.Action action(Context context, String drawable, String title, int code) {
        return new Notification.Action.Builder(
                        Icon.createWithResource(context, drawable(context, drawable)), title, pending(context, code))
                .build();
    }

    /** The same broadcast the picture-in-picture buttons send, so a tap means one thing. */
    private static PendingIntent pending(Context context, int code) {
        Intent intent = new Intent(UplinkActivity.ACTION_CALL)
                .setPackage(context.getPackageName())
                .putExtra(UplinkActivity.EXTRA_ACTION, code);
        return PendingIntent.getBroadcast(
                context, code, intent, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    /** By name: the resource table is ours, and there is no generated R class to look in. */
    private static int drawable(Context context, String name) {
        return context.getResources().getIdentifier(name, "drawable", context.getPackageName());
    }

    @Override
    public void onDestroy() {
        UplinkActivity.log(Log.INFO, "call service stopped");
        super.onDestroy();
    }
}
