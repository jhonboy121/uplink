package dev.uplink;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.os.IBinder;
import android.util.Log;

/**
 * Keeps a call running while the app is in the background: Android only lets a foreground service
 * with the camera and microphone types keep using them once the activity is no longer visible.
 */
public class UplinkCallService extends Service {
    private static final String CHANNEL = "call";
    private static final int NOTIFICATION_ID = 1;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        NotificationManager manager = getSystemService(NotificationManager.class);
        // Low importance: an ongoing call needs no sound of its own.
        NotificationChannel channel =
                new NotificationChannel(CHANNEL, "Calls", NotificationManager.IMPORTANCE_LOW);
        manager.createNotificationChannel(channel);

        Intent open = new Intent(this, UplinkActivity.class);
        PendingIntent tap = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
        Notification notification = new Notification.Builder(this, CHANNEL)
                // loadLabel, not getString(labelRes): our manifest has a literal label, no resources.
                .setContentTitle(getApplicationInfo().loadLabel(getPackageManager()))
                .setContentText("Call in progress")
                .setSmallIcon(android.R.drawable.presence_video_online)
                .setContentIntent(tap)
                .setOngoing(true)
                .build();
        startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA | ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE);
        UplinkActivity.log(Log.INFO, "call service started");
        // The activity stops it when the call ends; no restart if Android kills us.
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        UplinkActivity.log(Log.INFO, "call service stopped");
        super.onDestroy();
    }
}
