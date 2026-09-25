package dev.uplink;

import android.content.ComponentName;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.OutcomeReceiver;
import android.telecom.CallAudioState;
import android.telecom.CallEndpoint;
import android.telecom.CallEndpointException;
import android.telecom.Connection;
import android.telecom.ConnectionRequest;
import android.telecom.DisconnectCause;
import android.telecom.PhoneAccount;
import android.telecom.PhoneAccountHandle;
import android.telecom.TelecomManager;
import android.telecom.VideoProfile;
import android.util.Log;
import java.util.ArrayList;
import java.util.List;

/**
 * uplink's calls as Android's Telecom sees them: a self-managed phone account, and the one
 * connection for the call in progress. Telecom owns the audio mode, the outputs, the headset
 * buttons and hold; this is where each of those meets the core.
 *
 * <p>Telecom's no is final. A call it will not have is refused, never carried on without it.
 *
 * <p>Rust calls in from any thread and everything here is posted to the main thread, which is where
 * Telecom calls the connection back. What Rust reads back (hold, mute, the outputs) is kept in
 * volatile fields, replaced whole.
 */
final class UplinkTelecom {
  /** Must match `TelecomAction` in Rust. */
  static final int ANSWER = 0;

  static final int REJECT = 1;
  static final int HANGUP = 2;
  static final int REFUSED = 3;

  /**
   * Our own names for the outputs, because Android has two numberings for them (CallEndpoint from
   * 34, CallAudioState before). Must match `RouteKind` in Rust.
   */
  static final int ROUTE_PHONE = 0;

  static final int ROUTE_SPEAKER = 1;
  static final int ROUTE_BLUETOOTH = 2;
  static final int ROUTE_WIRED = 3;

  /** Not an output Telecom offers, but one of ours: their voice not played. */
  static final int OUTPUT_MUTE = 4;

  private static final int NO_ROUTE = -1;

  private static final String ACCOUNT_ID = "uplink";

  /** A key is not a phone number; Telecom only needs an address it can hand back. */
  private static final String SCHEME = "uplink";

  private static final String EXTRA_NAME = "dev.uplink.NAME";

  private final UplinkApplication app;
  private final Handler main = new Handler(Looper.getMainLooper());

  /** Null when Telecom is not there or would not register us: then every call is refused. */
  private final PhoneAccountHandle account;

  // Main thread only.
  private Connection connection;
  private String name = "";

  /**
   * A video call wants the speaker until the user picks an output. Kept as a wish, not done once:
   * Telecom puts its own default (the earpiece) back while ringing and again when the call goes
   * active, so it is re-applied on every report of the route.
   */
  private boolean wantSpeaker;

  /** An output change asked of Telecom (34+) and not yet answered. */
  private boolean switching;

  /** The outputs as Telecom last listed them, from 34, so a pick can be handed back. */
  private List<CallEndpoint> endpoints = new ArrayList<>();

  // Read by Rust.
  private volatile boolean held;
  private volatile boolean muted;
  private volatile int[] routeKinds = new int[0];
  private volatile String[] routeNames = new String[0];
  private volatile int route = NO_ROUTE;

  UplinkTelecom(UplinkApplication app) {
    this.app = app;
    this.account = register(app);
  }

  /**
   * Deprecated on purpose, twice. FEATURE_CONNECTION_SERVICE is FEATURE_TELECOM's older name, and
   * FEATURE_TELECOM only exists from 33 (we start at 30). CAPABILITY_SELF_MANAGED is the
   * ConnectionService route itself; its replacement, TelecomManager.addCall, only exists from 34.
   */
  @SuppressWarnings("deprecation")
  private static PhoneAccountHandle register(UplinkApplication app) {
    // Without either feature every TelecomManager call throws, registering included.
    PackageManager packages = app.getPackageManager();
    boolean supported =
        packages.hasSystemFeature(PackageManager.FEATURE_TELECOM)
            || packages.hasSystemFeature(PackageManager.FEATURE_CONNECTION_SERVICE);
    TelecomManager telecom = app.getSystemService(TelecomManager.class);
    if (!supported || telecom == null) {
      UplinkApplication.log(Log.ERROR, "telecom: not on this phone; calls will be refused");
      return null;
    }
    PhoneAccountHandle handle =
        new PhoneAccountHandle(new ComponentName(app, UplinkConnectionService.class), ACCOUNT_ID);
    PhoneAccount account =
        PhoneAccount.builder(handle, app.getApplicationInfo().loadLabel(app.getPackageManager()))
            .setCapabilities(
                PhoneAccount.CAPABILITY_SELF_MANAGED
                    | PhoneAccount.CAPABILITY_VIDEO_CALLING
                    | PhoneAccount.CAPABILITY_SUPPORTS_VIDEO_CALLING)
            .build();
    try {
      telecom.registerPhoneAccount(account);
      return handle;
    } catch (RuntimeException e) {
      UplinkApplication.log(
          Log.ERROR, "telecom: registering the account: " + e + "; calls will be refused");
      return null;
    }
  }

  private TelecomManager telecom() {
    return app.getSystemService(TelecomManager.class);
  }

  /** Whether Telecom would take a call now; false while an emergency call is up, say. */
  boolean permitted(boolean incoming) {
    if (account == null) {
      return false;
    }
    TelecomManager telecom = telecom();
    return incoming
        ? telecom.isIncomingCallPermitted(account)
        : telecom.isOutgoingCallPermitted(account);
  }

  /** The core is dialling `key`. Telecom answers through the service, or refuses. */
  void place(final String key, final String who, final boolean withVideo) {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            Bundle outgoing = new Bundle();
            outgoing.putString(EXTRA_NAME, who);
            name = who;
            Bundle extras = new Bundle();
            extras.putParcelable(TelecomManager.EXTRA_PHONE_ACCOUNT_HANDLE, account);
            extras.putInt(TelecomManager.EXTRA_START_CALL_WITH_VIDEO_STATE, videoState(withVideo));
            extras.putBundle(TelecomManager.EXTRA_OUTGOING_CALL_EXTRAS, outgoing);
            try {
              if (account == null) {
                throw new IllegalStateException("no phone account");
              }
              telecom().placeCall(Uri.fromParts(SCHEME, key, null), extras);
              UplinkApplication.log(Log.INFO, "telecom: placing, video " + withVideo);
            } catch (RuntimeException e) {
              UplinkApplication.log(Log.WARN, "telecom: placing: " + e);
              UplinkApplication.telecomAction(REFUSED);
            }
          }
        });
  }

  /** A call is ringing here. Telecom says when to show it, or refuses. */
  void incoming(final String key, final String who, final boolean withVideo) {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            Bundle incoming = new Bundle();
            incoming.putString(EXTRA_NAME, who);
            name = who;
            Bundle extras = new Bundle();
            extras.putParcelable(
                TelecomManager.EXTRA_INCOMING_CALL_ADDRESS, Uri.fromParts(SCHEME, key, null));
            extras.putInt(TelecomManager.EXTRA_INCOMING_VIDEO_STATE, videoState(withVideo));
            extras.putBundle(TelecomManager.EXTRA_INCOMING_CALL_EXTRAS, incoming);
            try {
              if (account == null) {
                throw new IllegalStateException("no phone account");
              }
              telecom().addNewIncomingCall(account, extras);
              UplinkApplication.log(Log.INFO, "telecom: incoming, video " + withVideo);
            } catch (RuntimeException e) {
              UplinkApplication.log(Log.WARN, "telecom: incoming: " + e);
              UplinkApplication.telecomAction(REFUSED);
            }
          }
        });
  }

  private static int videoState(boolean withVideo) {
    return withVideo ? VideoProfile.STATE_BIDIRECTIONAL : VideoProfile.STATE_AUDIO_ONLY;
  }

  // ---- The service's half -----------------------------------------------------------------

  /** Telecom took the call: this connection is it from now on. Main thread. */
  Connection created(ConnectionRequest request, boolean incoming) {
    Bundle extras = request.getExtras();
    // Kept from when we asked, because what comes back is not the bundle we sent: an incoming
    // request's extras hold ours nested, and an empty name is a notification Android refuses.
    String given = extras != null ? extras.getString(EXTRA_NAME, "") : "";
    if (!given.isEmpty()) {
      name = given;
    }
    wantSpeaker = request.getVideoState() != VideoProfile.STATE_AUDIO_ONLY;
    if (connection != null) {
      // One call at a time: whatever was left of the last one is over.
      connection.setDisconnected(new DisconnectCause(DisconnectCause.OTHER));
      connection.destroy();
    }
    held = false;
    muted = false;
    UplinkConnection made = new UplinkConnection(this);
    made.setConnectionProperties(Connection.PROPERTY_SELF_MANAGED);
    made.setConnectionCapabilities(Connection.CAPABILITY_HOLD | Connection.CAPABILITY_SUPPORT_HOLD);
    made.setAudioModeIsVoip(true);
    made.setAddress(request.getAddress(), TelecomManager.PRESENTATION_ALLOWED);
    made.setCallerDisplayName(name, TelecomManager.PRESENTATION_ALLOWED);
    made.setVideoState(request.getVideoState());
    if (incoming) {
      made.setRinging();
    } else {
      made.setDialing();
    }
    connection = made;
    UplinkApplication.log(Log.INFO, "telecom: connection made, incoming " + incoming);
    return made;
  }

  /** Telecom would not have the call. Main thread. */
  void failed(boolean incoming) {
    UplinkApplication.log(
        Log.WARN, "telecom: refused the " + (incoming ? "incoming" : "outgoing") + " call");
    UplinkApplication.telecomAction(REFUSED);
  }

  // ---- The connection's half --------------------------------------------------------------

  void showIncoming() {
    boolean video =
        connection != null && connection.getVideoState() != VideoProfile.STATE_AUDIO_ONLY;
    app.ring(name, video);
  }

  void silence() {
    app.silenceRinging();
  }

  void action(int action) {
    UplinkApplication.telecomAction(action);
  }

  void hold(Connection from, boolean on) {
    if (from != connection) {
      return;
    }
    if (on) {
      from.setOnHold();
    } else {
      from.setActive();
    }
    held = on;
    UplinkApplication.log(Log.INFO, "telecom: " + (on ? "held" : "resumed"));
    app.callAudioChanged();
  }

  void mute(Connection from, boolean on) {
    if (from != connection || muted == on) {
      return;
    }
    muted = on;
    app.callAudioChanged();
  }

  /** The outputs from 34, with the one in use. */
  void endpoints(Connection from, List<CallEndpoint> available, CallEndpoint current) {
    if (from != connection || Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      return;
    }
    if (available != null) {
      endpoints = new ArrayList<>(available);
    }
    int[] kinds = new int[endpoints.size()];
    String[] names = new String[endpoints.size()];
    int now = NO_ROUTE;
    for (int i = 0; i < endpoints.size(); i++) {
      CallEndpoint endpoint = endpoints.get(i);
      kinds[i] = kind(endpoint.getEndpointType());
      names[i] =
          endpoint.getEndpointType() == CallEndpoint.TYPE_BLUETOOTH
              ? String.valueOf(endpoint.getEndpointName())
              : "";
      if (current != null && current.getIdentifier().equals(endpoint.getIdentifier())) {
        now = i;
      }
    }
    if (current == null) {
      now = route;
    }
    publish(kinds, names, now);
  }

  private static int kind(int endpointType) {
    switch (endpointType) {
      case CallEndpoint.TYPE_SPEAKER:
        return ROUTE_SPEAKER;
      case CallEndpoint.TYPE_BLUETOOTH:
        return ROUTE_BLUETOOTH;
      case CallEndpoint.TYPE_WIRED_HEADSET:
        return ROUTE_WIRED;
      default:
        return ROUTE_PHONE;
    }
  }

  /**
   * The outputs before 34. Bluetooth is one row: a headset's name needs a runtime permission there,
   * and Telecom's route switch picks the active headset anyway.
   */
  void audioState(Connection from, CallAudioState state) {
    if (from != connection || Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      return;
    }
    int[] order = {
      CallAudioState.ROUTE_EARPIECE,
      CallAudioState.ROUTE_WIRED_HEADSET,
      CallAudioState.ROUTE_SPEAKER,
      CallAudioState.ROUTE_BLUETOOTH
    };
    int[] ours = {ROUTE_PHONE, ROUTE_WIRED, ROUTE_SPEAKER, ROUTE_BLUETOOTH};
    List<Integer> kinds = new ArrayList<>();
    int now = NO_ROUTE;
    for (int i = 0; i < order.length; i++) {
      if ((state.getSupportedRouteMask() & order[i]) != 0) {
        if (state.getRoute() == order[i]) {
          now = kinds.size();
        }
        kinds.add(ours[i]);
      }
    }
    int[] packed = new int[kinds.size()];
    for (int i = 0; i < packed.length; i++) {
      packed[i] = kinds.get(i);
    }
    muted = state.isMuted();
    publish(packed, new String[packed.length], now);
  }

  private void publish(int[] kinds, String[] names, int now) {
    for (int i = 0; i < names.length; i++) {
      if (names[i] == null) {
        names[i] = "";
      }
    }
    routeKinds = kinds;
    routeNames = names;
    route = now;
    keepSpeaker();
    app.callAudioChanged();
  }

  // ---- The core's half --------------------------------------------------------------------

  /** Connected. */
  void active() {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            if (connection != null) {
              connection.setActive();
            }
          }
        });
  }

  /** A voice call became video: the speaker, as a video call would have started. */
  void setVideo(final boolean on) {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            wantSpeaker = on;
            if (connection == null) {
              return;
            }
            connection.setVideoState(videoState(on));
            keepSpeaker();
          }
        });
  }

  /**
   * Moves a video call off the earpiece onto the speaker, unless a headset is connected, which is
   * where the user would want it anyway.
   */
  private void keepSpeaker() {
    int now = route;
    int[] kinds = routeKinds;
    // One request at a time: the list and the current output are reported one after the
    // other, and a second request cancels the first.
    if (!wantSpeaker || switching || now < 0 || now >= kinds.length || kinds[now] != ROUTE_PHONE) {
      return;
    }
    int speaker = NO_ROUTE;
    for (int i = 0; i < kinds.length; i++) {
      if (kinds[i] == ROUTE_BLUETOOTH || kinds[i] == ROUTE_WIRED) {
        return;
      }
      if (kinds[i] == ROUTE_SPEAKER) {
        speaker = i;
      }
    }
    if (speaker != NO_ROUTE) {
      choose(speaker);
    }
  }

  /** The call is over, for `cause` (a `DisconnectCause` code, read by Rust from the SDK). */
  void ended(final int cause) {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            held = false;
            muted = false;
            routeKinds = new int[0];
            routeNames = new String[0];
            route = NO_ROUTE;
            endpoints = new ArrayList<>();
            switching = false;
            if (connection == null) {
              return;
            }
            connection.setDisconnected(new DisconnectCause(cause));
            connection.destroy();
            connection = null;
          }
        });
  }

  /** One of the outputs {@link #routeKinds} lists. */
  void route(final int index) {
    main.post(
        new Runnable() {
          @Override
          public void run() {
            // The user chose: from here on, wherever they put it is where it stays.
            wantSpeaker = false;
            choose(index);
          }
        });
  }

  /** setAudioRoute is deprecated from 34, where CallEndpoint replaces it; 30–33 have only it. */
  @SuppressWarnings("deprecation")
  private void choose(int index) {
    if (connection == null || index < 0 || index >= routeKinds.length) {
      return;
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
      if (index >= endpoints.size()) {
        return;
      }
      switching = true;
      connection.requestCallEndpointChange(
          endpoints.get(index),
          app.getMainExecutor(),
          new OutcomeReceiver<Void, CallEndpointException>() {
            @Override
            public void onResult(Void result) {
              switching = false;
            }

            @Override
            public void onError(CallEndpointException e) {
              switching = false;
              UplinkApplication.log(Log.WARN, "telecom: changing output: " + e);
            }
          });
      return;
    }
    // Indexed by our kinds: ROUTE_PHONE, ROUTE_SPEAKER, ROUTE_BLUETOOTH, ROUTE_WIRED.
    int[] routes = {
      CallAudioState.ROUTE_EARPIECE,
      CallAudioState.ROUTE_SPEAKER,
      CallAudioState.ROUTE_BLUETOOTH,
      CallAudioState.ROUTE_WIRED_HEADSET
    };
    connection.setAudioRoute(routes[routeKinds[index]]);
  }

  boolean held() {
    return held;
  }

  boolean muted() {
    return muted;
  }

  int[] routeKinds() {
    return routeKinds;
  }

  String[] routeNames() {
    return routeNames;
  }

  int route() {
    return route;
  }
}
