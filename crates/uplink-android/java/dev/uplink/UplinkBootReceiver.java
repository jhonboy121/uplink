package dev.uplink;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/**
 * Brings the listening service back after a reboot, and after an update — installing a new
 * version kills the old process, and otherwise nothing would ring until someone opened the app.
 *
 * <p>Both broadcasts are on Android's short list of moments an app may start a foreground
 * service from the background. From 14 that no longer covers a microphone service started at
 * boot, and from 15 not a camera one either, which is why the listener is `specialUse` and holds
 * neither. An app that has never been opened, or that was force-stopped, is sent neither
 * broadcast; that first launch is on the user.
 */
public class UplinkBootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context context, Intent intent) {
        String action = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action) && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(action)) {
            return;
        }
        UplinkActivity.log(Log.INFO, "starting to listen after " + action);
        try {
            context.startForegroundService(new Intent(context, UplinkListenService.class));
        } catch (RuntimeException e) {
            // ForegroundServiceStartNotAllowedException on 12+, if the system disagrees about
            // which moment this is. There is nothing to retry with; the next launch will do.
            UplinkActivity.log(Log.WARN, "could not start listening: " + e);
        }
    }
}
