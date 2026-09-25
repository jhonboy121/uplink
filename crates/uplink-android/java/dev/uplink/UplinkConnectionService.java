package dev.uplink;

import android.telecom.Connection;
import android.telecom.ConnectionRequest;
import android.telecom.ConnectionService;
import android.telecom.PhoneAccountHandle;

/**
 * What Telecom binds for each call. It only hands over: the connection and its state live on {@link
 * UplinkTelecom}, which the Application owns for the life of the process.
 */
public class UplinkConnectionService extends ConnectionService {
  private UplinkTelecom telecom() {
    return ((UplinkApplication) getApplicationContext()).telecom();
  }

  @Override
  public Connection onCreateOutgoingConnection(
      PhoneAccountHandle account, ConnectionRequest request) {
    return telecom().created(request, false);
  }

  @Override
  public void onCreateOutgoingConnectionFailed(
      PhoneAccountHandle account, ConnectionRequest request) {
    telecom().failed(false);
  }

  @Override
  public Connection onCreateIncomingConnection(
      PhoneAccountHandle account, ConnectionRequest request) {
    return telecom().created(request, true);
  }

  @Override
  public void onCreateIncomingConnectionFailed(
      PhoneAccountHandle account, ConnectionRequest request) {
    telecom().failed(true);
  }
}
