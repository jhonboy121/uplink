# Telecom (self-managed ConnectionService)

How uplink's calls go through Android's Telecom, and the quirks found before building it
(2026-09-25). Plain framework APIs (no androidx `core-telecom`: that is Kotlin plus Gradle).
Classes: `UplinkTelecom` (account, connection, route/hold/mute state), `UplinkConnection`,
`UplinkConnectionService`; Rust side in `uplink-android/src/platform.rs` (`telecom_*`).

## The shape

- One `PhoneAccount`, `CAPABILITY_SELF_MANAGED | CAPABILITY_VIDEO_CALLING |
  CAPABILITY_SUPPORTS_VIDEO_CALLING`, registered in `Application.onCreate`.
- Placing: `isOutgoingCallPermitted` → `placeCall(uplink:<key>)` → `onCreateOutgoingConnection`
  (`setDialing`) → Connected → `setActive`.
- Receiving: `addNewIncomingCall` → `onCreateIncomingConnection` (`setRinging`) →
  `onShowIncomingCallUi` rings (our notification + ringtone; a self-managed app draws its own).
- Telecom's no is final (user decision): refused or failed ⇒ `Command::Refused` (a ringing call
  answers Busy, a placed one hangs up).
- Hold: `CAPABILITY_HOLD | CAPABILITY_SUPPORT_HOLD`; `onHold` → `setOnHold`, `onUnhold` → `setActive`.
  The peer is told via `MediaState.held`.
- Outputs: `CallEndpoint` + `requestCallEndpointChange` from 34; `CallAudioState` +
  `setAudioRoute` on 30–33 (Bluetooth is one row there: a headset's name needs
  `BLUETOOTH_CONNECT`).
- Verified in android.jar (37) with javap: every API above, and the constants used.

## Quirks handled (verified, with a known fix)

| Quirk | Fix | Source |
|---|---|---|
| Every `TelecomManager` call throws on a device without `FEATURE_TELECOM` / `FEATURE_CONNECTION_SERVICE` | Check both features before registering; no account ⇒ calls refused | [Voximplant notes](https://github.com/voximplant/android-sdk-kotlin-demo/blob/master/audiocall/ConnectionService.MD) |
| Registration itself can be rejected by the system | Caught; treated as "Telecom said no" for the process (Signal does the same: `systemRejected`) | Signal `AndroidTelecomUtil.kt` |
| API 34+: microphone input lost with the screen locked / app backgrounded during a VoIP call (Pixel 7/8, S23) | Call service declares and starts with `phoneCall` type (`FOREGROUND_SERVICE_PHONE_CALL`, needs only `MANAGE_OWN_CALLS`); `phoneCall` alone is the fallback when camera/mic are refused from the background | [twilio/audioswitch#158](https://github.com/twilio/audioswitch/issues/158), [FGS types](https://developer.android.com/develop/background-work/services/fgs/service-types) |
| Telecom puts its default route (earpiece, with no headset) back while ringing and again on `STATE_ACTIVE`, so a video call lands on the earpiece | "Want speaker" kept for a video call and re-applied on every route report, unless a headset is connected; dropped once the user picks | [flutter_callkit_incoming PR 3](https://github.com/AlexBacich/flutter_callkit_incoming/pull/3), [jitsi-meet#3912](https://github.com/jitsi/jitsi-meet/issues/3912) |
| A cellular call answered over ours: held from API 28, dropped on 27 and lower; the app must set the hold capabilities itself | Both capabilities set; minSdk 31 | [AOSP third-party call apps](https://source.android.com/docs/core/connect/third-party-call-apps), Voximplant notes |
| `phoneCall` FGS may not be started from `BOOT_COMPLETED` on 15+ | Only the listening service starts at boot (`specialUse`); the call service never does | FGS types page |

## Known, not handled (no verified fix)

- Some Samsung **tablets** (SM-T290) never call `onCreateOutgoingConnection` for `placeCall`
  (voice calling disabled). No resolution published. Our outgoing call would dial with no
  connection; watch the log for "telecom: placing" without "connection made".
- Signal ships Telecom behind a remote flag (`useJetPackTelecom`) and a min-SDK flag, i.e. staged,
  not on for everyone. Jitsi turned ConnectionService off by default. Neither says why in public.
- androidx core-telecom 1.19.0-alpha02 fixed audio calls routing to the speaker at start on 14–16
  (b/491932378), in its own transactional path. Unknown whether plain ConnectionService is hit.
