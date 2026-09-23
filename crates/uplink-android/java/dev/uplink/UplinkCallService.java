package dev.uplink;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Person;
import android.app.Service;
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
        createChannel();
        startForeground(
                NOTIFICATION_ID,
                notification(peer != null ? peer : "", micOn),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA | ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
        UplinkActivity.log(Log.INFO, "call service started");
        // The activity stops it when the call ends; no restart if Android kills us.
        return START_NOT_STICKY;
    }

    /**
     * Default importance rather than low, because a call that is demoted shows as an ordinary
     * line instead of a call — but silent, since the call itself is the sound.
     */
    private void createChannel() {
        NotificationChannel channel =
                new NotificationChannel(CHANNEL, "Calls", NotificationManager.IMPORTANCE_DEFAULT);
        channel.setSound(null, null);
        channel.enableVibration(false);
        getSystemService(NotificationManager.class).createNotificationChannel(channel);
    }

    private Notification notification(String peer, boolean micOn) {
        Intent open = new Intent(this, UplinkActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification.Builder builder = new Notification.Builder(this, CHANNEL)
                // Our own mark, cropped to its bounds: the system draws a small icon flat from
                // its alpha, so it is a silhouette that should fill the box rather than sit in
                // a launcher icon's safe zone.
                .setSmallIcon(drawable("notification"))
                .setContentIntent(tap)
                .setOngoing(true)
                .setColor(ACCENT)
                .setColorized(true);
        Notification.Action mute = action(micOn ? "mic" : "mic_off", micOn ? "Mute" : "Unmute", UplinkActivity.ACTION_MIC);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Names the person and owns the hang-up itself; the mute rides along beside it.
            Person who = new Person.Builder().setName(peer).setImportant(true).build();
            builder.setStyle(Notification.CallStyle.forOngoingCall(who, pending(UplinkActivity.ACTION_HANGUP)))
                    .addAction(mute);
        } else {
            // loadLabel, not getString(labelRes): our manifest has a literal label, no resources.
            builder.setContentTitle(peer.isEmpty() ? getApplicationInfo().loadLabel(getPackageManager()) : peer)
                    .setContentText("Call in progress")
                    .addAction(action("call_end", "End call", UplinkActivity.ACTION_HANGUP))
                    .addAction(mute);
        }
        return builder.build();
    }

    private Notification.Action action(String drawable, String title, int code) {
        return new Notification.Action.Builder(
                        Icon.createWithResource(this, drawable(drawable)), title, pending(code))
                .build();
    }

    /** The same broadcast the picture-in-picture buttons send, so a tap means one thing. */
    private PendingIntent pending(int code) {
        Intent intent = new Intent(UplinkActivity.ACTION_CALL)
                .setPackage(getPackageName())
                .putExtra(UplinkActivity.EXTRA_ACTION, code);
        return PendingIntent.getBroadcast(
                this, code, intent, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
    }

    /** By name: the resource table is ours, and there is no generated R class to look in. */
    private int drawable(String name) {
        return getResources().getIdentifier(name, "drawable", getPackageName());
    }

    @Override
    public void onDestroy() {
        UplinkActivity.log(Log.INFO, "call service stopped");
        super.onDestroy();
    }
}
