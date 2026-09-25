package dev.uplink;

import android.telecom.CallAudioState;
import android.telecom.CallEndpoint;
import android.telecom.Connection;

import java.util.List;

/**
 * The call in progress, as Telecom holds it. Everything Telecom asks of it — answer, reject, hang
 * up, hold, and what the outputs and mute are doing — is passed to {@link UplinkTelecom}, which
 * turns it into what the core and the window understand. Main thread, as Telecom calls it.
 */
final class UplinkConnection extends Connection {
    private final UplinkTelecom telecom;
    /** Kept so either change can be published with the other half it belongs to. */
    private List<CallEndpoint> available;
    private CallEndpoint current;

    UplinkConnection(UplinkTelecom telecom) {
        this.telecom = telecom;
    }

    /** A self-managed app draws its own ringing; this is Telecom saying now is the time. */
    @Override
    public void onShowIncomingCallUi() {
        telecom.showIncoming();
    }

    /** The volume key while it rings: quiet, but still ringing. */
    @Override
    public void onSilence() {
        telecom.silence();
    }

    @Override
    public void onAnswer() {
        telecom.action(UplinkTelecom.ANSWER);
    }

    @Override
    public void onAnswer(int videoState) {
        telecom.action(UplinkTelecom.ANSWER);
    }

    @Override
    public void onReject() {
        telecom.action(UplinkTelecom.REJECT);
    }

    @Override
    public void onDisconnect() {
        telecom.action(UplinkTelecom.HANGUP);
    }

    @Override
    public void onAbort() {
        telecom.action(UplinkTelecom.HANGUP);
    }

    /** A phone call was answered over ours. */
    @Override
    public void onHold() {
        telecom.hold(this, true);
    }

    @Override
    public void onUnhold() {
        telecom.hold(this, false);
    }

    @Override
    public void onMuteStateChanged(boolean isMuted) {
        telecom.mute(this, isMuted);
    }

    @Override
    public void onAvailableCallEndpointsChanged(List<CallEndpoint> endpoints) {
        available = endpoints;
        telecom.endpoints(this, endpoints, current);
    }

    @Override
    public void onCallEndpointChanged(CallEndpoint endpoint) {
        current = endpoint;
        telecom.endpoints(this, available, endpoint);
    }

    /**
     * Before 34, one callback carries the outputs, the one in use and the mute. Deprecated from
     * 34, where the CallEndpoint callbacks above replace it; 30–33 have only this.
     */
    @Override
    @SuppressWarnings("deprecation")
    public void onCallAudioStateChanged(CallAudioState state) {
        telecom.audioState(this, state);
    }
}
