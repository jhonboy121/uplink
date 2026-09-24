package dev.uplink;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps the process alive so the endpoint stays bound and a call has somewhere to arrive.
 *
 * <p>This is not the call service. That one holds the camera and microphone and only runs during
 * a call; this one holds nothing and runs always, because swiping the app out of recents kills a
 * process that has no foreground service, and with the process goes the endpoint.
 *
 * <p>Its type is `specialUse` for two reasons: none of the others honestly describes waiting for
 * a call, and it is the only type Android still lets a BOOT_COMPLETED receiver start — 14 blocks
 * microphone that way and 15 adds camera and the rest.
 */
public class UplinkListenService extends Service {
    private static final String CHANNEL = "ready";
    private static final int NOTIFICATION_ID = 2;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        createChannel();
        startForeground(NOTIFICATION_ID, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        UplinkActivity.log(Log.INFO, "listening");
        // Restarted if Android ever does kill us: being reachable is the whole point of it.
        return START_STICKY;
    }

    /**
     * Lowest importance there is: this notification says nothing is happening, and the user did
     * not ask to be told that. Android insists a foreground service post something.
     */
    private void createChannel() {
        NotificationChannel channel = new NotificationChannel(CHANNEL, "Ready for calls", NotificationManager.IMPORTANCE_MIN);
        channel.setShowBadge(false);
        channel.setSound(null, null);
        getSystemService(NotificationManager.class).createNotificationChannel(channel);
    }

    private Notification notification() {
        Intent open = new Intent(this, UplinkActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification.Builder builder = new Notification.Builder(this, CHANNEL)
                .setSmallIcon(getResources().getIdentifier("notification", "drawable", getPackageName()))
                .setContentTitle("Ready for calls")
                .setContentText("uplink is reachable")
                .setContentIntent(tap)
                .setOngoing(true);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            // Android holds a foreground service's notification back for up to ten seconds, so
            // that a service which starts and finishes in that time never flashes one up. This
            // one lives as long as the app does, and the wait reads as uplink taking that long
            // to come online when it has in fact been reachable the whole time.
            builder.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        }
        return builder.build();
    }

    @Override
    public void onDestroy() {
        UplinkActivity.log(Log.INFO, "no longer listening");
        super.onDestroy();
    }
}
