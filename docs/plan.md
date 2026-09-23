# uplink — plan

P2P, end-to-end encrypted, minimal video calling for Android, iOS and web. Android first.
This is the source of truth for decisions. Changes go in as dated entries in [Revisions](#revisions).

## Stack

| Area | Decision |
|---|---|
| Language | Rust everywhere; no Flutter, KMP, Expo, Capacitor or webviews |
| UI | Slint 1.18 (Android backend always renders with Skia/GLES) |
| Android glue | android-activity (NativeActivity) + forked `ndk`/`ndk-sys` (bindings generated from NDK r30), `ndk-gl-sys` (EGL/GLES3) |
| Transport, discovery | iroh: dial by public key; relay fallback; DNS discovery |
| Video | platform hardware codecs (MediaCodec / VideoToolbox / WebCodecs), H.264; no software codec |
| Audio | platform I/O with system echo cancellation (AAudio voice-communication preset, iOS VoiceProcessingIO, browser `echoCancellation`) + Opus |
| Camera → UI | zero-copy: AImageReader (PRIVATE, GPU_SAMPLED) → EGLImage → external-OES blit → Slint borrowed GL texture |

## Architecture

- `uplink-core`: platform-agnostic and async (tokio). Handles identity, contacts, iroh endpoint, call signalling and
  its state machine, and media framing. API = commands in, event stream out. Uses `tokio::fs` for storage. It is testable
  on the host through a CLI.
- **Runtime:** tokio, built explicitly and owned by the app/CLI (no `#[tokio::main]`, no statics).
- **UI bridge:** UI → core over `mpsc` channels (or spawn on the runtime `Handle`). Core → UI via
  `slint::Weak::upgrade_in_event_loop`. `slint::spawn_local` only awaits runtime-agnostic futures (oneshot, JoinHandle).
- **Media never crosses the UI thread:** encoder callbacks → channel → iroh stream; network → decoder → preview.
- **Media framing (ours, over iroh):** one QUIC stream per track (QUIC datagrams as an option for video). Small header:
  track, timestamp, keyframe flag.
- **Platform bridge (Android):**
  - `UplinkActivity extends NativeActivity` (plain Java) adds the callbacks NativeActivity lacks (permission results,
    activity results, new intents; later the foreground call service).
  - Context passing without globals: a Rust-owned `Arc<Inner>` is handed to the activity as a `jlong` handle, and Java
    passes it back on every callback. `onDestroy` → `nativeDetach` releases it.
  - Natives are registered with `RegisterNatives` on the activity's class (no exported `Java_*` symbols; class-loader safe).
  - Requests are awaitable: request code → `oneshot` (`platform.request_permission(Camera).await`). Unsolicited
    platform events go on an `mpsc`. Main-thread-only Java calls use `runOnUiThread` inside the Java helpers.
  - `Platform` owns its `JavaVM` and a global ref to the activity; no `JavaVM::singleton()`.
  - Java logs via `android.util.Log`, tag `uplink`.

## Identity, contacts, infrastructure

- Identity is an iroh keypair per device, plus a **profile**: a display name and an optional picture. No accounts,
  phone numbers or servers of our own.
- **An identity is the key and the display name**, shared as a code or a link and saved as a contact outright. The
  picture does not fit — a QR holds ~2.9 KB and a 256px picture is an order of magnitude more — so it arrives over the
  connection instead, on first contact and whenever it changes.
- **A name is a claim.** Contacts keep a **local nickname** that always wins; a scanned name only pre-fills it. A caller
  whose key is not saved is shown as a quoted claim with the fingerprint under it and **no picture**, because a
  photograph is the most convincing of the three lies.
- Storage is **SQLite via `rusqlite`** (bundled, built with our clang wrapper like libopus). A contact is key, nickname,
  advertised name, picture hash, favourite and last-called. **Pictures are files in the app-private directory named by
  their hash**, never BLOBs and never external storage; the row holds only the hash, so one file serves every contact
  that sends it and a change is a new file rather than an overwrite mid-read.
- **Privacy switches:** *share my profile* (off ⇒ the code is key-only and calls advertise nothing) and *reject unknown
  callers* (a key not in contacts never rings; refused at signalling and logged). Every call advertises the profile when
  sharing is on, which is what lets an unknown caller say who they are.
- Relays and discovery: n0 public infrastructure for now, kept configurable. Before real users, measure the share of
  relayed calls, then self-host `iroh-relay` / `iroh-dns-server` or use n0's paid hosting (public relays are best-effort
  with no SLA; check n0's terms).

## Security (E2EE)

- The QUIC/TLS 1.3 session runs **directly between the two devices** and is authenticated by their iroh keys. Relays
  only forward ciphertext, so the transport is end-to-end, with per-connection forward secrecy (ephemeral ECDHE).
  No extra encryption layer for 1:1 calls.
- To do:
  - **Verification UX:** a short fingerprint/safety code to compare; in-person QR scan is the strong path; warn when a
    contact's key changes.
  - **Key at rest:** the iroh secret key is encrypted with an Android Keystore key, never stored in plaintext.
  - ~~**Post-quantum**~~ done: aws-lc-rs provider, peer connections must negotiate `X25519MLKEM768` (see Revisions).
  - **Metadata:** relays and discovery see who/when/how much (never content). Self-hosting reduces third-party exposure.
- Not needed yet: X3DH/Double Ratchet (only for offline stored messages; we store nothing server-side). SFrame
  (only if a server terminating media is added for group calls; P2P mesh stays E2E).

## Build and tooling

- `just` is the entry point: `bindgen`, `check`, `clippy`, `build`, `apk`, `fmt`, `size`, `clean`; overridable
  `profile`, `log`, SDK levels, app id. Target SDK 37, min SDK 30.
- APK: `axml` (our manifest writer, replaces x86-only aapt2) + `jar` + `apksigner.jar`. The Java shim adds `javac` +
  `d8.jar` → `classes.dex`.
- **No Gradle, no xbuild** (unmaintained; its Java path is Gradle). A Rust APK signer can replace `jar`/`apksigner` later.
- Development happens on-device (Termux + proot Alpine, arm64 musl). No adb: apps log to logcat + a data-dir file,
  and show the previous run's log and exit reasons (ApplicationExitInfo) in-app. iOS builds will need macOS CI.

## Engineering rules

No statics (incl. `thread_local!`); `const fn` where possible; no magic numbers (named consts or values read from the
source of truth: generated bindings, JNI static fields); terse comments; clippy/rustc clean without `allow` (except bindgen
naming lints in generated `-sys` crates); no `unwrap`/`expect`; `anyhow` in binaries, `thiserror` in libraries;
`tracing` for logs, used explicitly (a `Dispatch` scoped per `android_main`, handed to other threads; never a global
default); keep disk usage lean. Avoid build scripts.

## Roadmap

1. **Platform bridge**: `UplinkActivity` + `Platform` (awaitable permission request, no press-Start-again), Java
   build steps in `just apk`.
2. **`uplink-core` + CLI**: identity, contacts, iroh endpoint, call signalling; two CLI instances call each other.
3. **iroh in the app**: the app calls/answers the CLI peer (proves iroh + rustls on `aarch64-linux-android`).
4. **Video**: camera → MediaCodec encoder (input surface) → iroh → MediaCodec decoder → ImageReader → preview.
5. **Audio**: AAudio voice-communication + Opus on its own stream.
6. **Call service**: foreground service so calls survive backgrounding.
7. **Product UI** (design locked, see Revisions): 7a contacts + QR + call screens **done**, matched to the design and
   themed. 7b: the shell (People / Calls / Connect / Settings), the splash, the permission gate, `rusqlite` contacts
   and the call log — **done**; still open are the contact screen (favourite, rename, remove), ringback and ringtone
   with an outgoing timeout, the profile over the wire, and the privacy switches. 7c PiP, lock-screen calls,
   diagnostics report; 7d verification, identity rotation.
8. ~~**Resource table**~~ **done**: `tools/android-res` compiles `android/` into the manifest, `resources.arsc` and the
   APK itself. Launcher icon and system splash confirmed on the S24. See [docs/ref/android-res.md](ref/android-res.md).
9. Security to-dos above; then iOS (CI) and web.

## Revisions

- **2026-09-22**: initial plan. Spikes done: on-device APK toolchain, Slint on Android (IME, lifecycle, diagnostics),
  vendored ndk with r30 bindings, zero-copy camera preview (30 fps, 10–20% of one core, debug build). Locked: stack,
  tokio core + UI bridge, own framing over iroh, n0 relays for now, key-only contacts, platform bridge design,
  E2EE position, no Gradle/xbuild.
- **2026-09-22**: roadmap step 1 done (platform bridge). Logging is explicit `tracing` (not slog, since deps log via tracing):
  `log::init` returns a `Dispatch`, scoped with `set_default` per `android_main` run and passed to other threads/the panic
  hook. Android reuses processes, so `android_main` can run repeatedly and process-wide set-once state breaks relaunch.
  JNI handles use bit-preserving `cast_signed`/`cast_unsigned` (arm64 tagged pointers). adb over localhost is used for
  debugging when available (`just run`, `just logcat`).
- **2026-09-22**: roadmap step 2 (`uplink-core` + CLI) implemented. iroh 1.2 with **aws-lc-rs** (not ring). Peer connections
  **must** negotiate hybrid `X25519MLKEM768`; anything else is refused before signalling (`CLOSE_NOT_POST_QUANTUM`). X25519
  stays in the provider only for relay/HTTPS. aws-lc builds for Android via our clang wrapper without system CMake.
  `Node::start(secret, Network::{N0, Local(MemoryLookup)})`; Local = loopback + in-memory lookup for tests. Tests: unit +
  loopback integration (`just test`), coverage with `cargo-llvm-cov` + system LLVM 22 tools (`just coverage`); tests use
  `anyhow::Result` + `?` (no unwrap/expect, no clippy test exceptions).
- **2026-09-22**: roadmap step 3 done: the app runs uplink-core over n0 and calls/answers the CLI in both directions on
  the S24. `NodeHandle` holds a `WeakSender` (non-owning; only `Node` controls the node's lifetime, `shutdown` = drop the
  strong sender + await the engine). UI is state-driven (`CallState` from node events, optimistic "Calling…", spinner);
  richer call screens stay in step 7. Diagnostics live behind a Log overlay, not on the main screen.
- **2026-09-22**: **media transport locked** (modelled on RTP/WebRTC practice, mapped onto QUIC as in IETF MoQ / RoQ):
  - **Video:** one unidirectional QUIC stream per encoded frame (length-prefixed postcard header: sequence, capture timestamp,
    keyframe/config flags; then the payload). Frames are reliable individually but never block each other. Frames past
    their deadline are reset by the sender and dropped by the receiver. The receiver delivers in sequence order; on a gap it
    drops until the next keyframe and sends `KeyframeRequest` (rate-limited) over the signalling stream.
  - **Audio:** QUIC datagrams, one Opus packet each; loss handled by Opus PLC/FEC, never retransmitted; highest priority.
  - **Rate control:** encoder bitrate follows QUIC's RTT/loss/cwnd (`connection.stats()`), below the congestion estimate.
    Start with Cubic (iroh default); compare **BBRv3** (`noq::congestion::Bbr3Config`) under load before switching.
  - **Jitter buffer + A/V sync** from capture timestamps, audio as the clock.
  - **Telemetry is part of the design:** remote testing (another country, no adb) means every call records periodic stats
    (path direct/relay, RTT, loss, bitrate, frames sent/received/dropped, keyframe requests), exchanged with the peer so
    each device keeps **both sides**, persisted as per-call reports in the data dir and viewable in-app.
  - Step 4 order: 4a core media transport (host-tested with synthetic frames) → 4b Android MediaCodec encode (camera input
    surface) / decode (into the zero-copy preview) → 4c call telemetry + reports.
- **2026-09-22**: step 4b done. It was tested against the CLI (`--video clip.mp4`, which sends an H.264 mp4's
  samples as-is, and prints the video it receives each second): 1080p clip → phone, and phone 720p30 → CLI, with no
  drops. The CLI uses clap. The camera feeds two surfaces (preview ImageReader + encoder input surface). The codecs run
  in MediaCodec async mode. The NDK callbacks feed a channel, and one tokio task per codec drives it. Nothing polls and
  there are no dedicated threads. A `CancellationToken` stops the tasks, and the call waits for them before its
  surfaces are released. `MediaCodec` gets one reasoned `unsafe impl Send` wrapper: `AMediaCodec` is locked internally,
  and async mode is already cross-thread by design. The decoder task takes a frame only while it has a free input
  buffer, so any backlog falls to uplink-core's drop/resync/keyframe-request path. The decoder renders into a second
  ImageReader that goes through the same zero-copy blit. SPS/PPS are prepended to every keyframe, so any keyframe can
  start decoding. The frame header carries `turns` (quarter turns to upright);
  mirroring stays local to the self-view. 720p30, 2 Mbps, 2 s keyframe interval until rate control lands. The mime and
  `COLOR_FormatSurface` are read over JNI. The `ndk` feature is `api-level-30` (= minSdk).
- **2026-09-22**: step 5 (audio) implemented, not yet device-tested. **Opus is libopus**, not the platform codec:
  in-band FEC, packet-loss concealment and DTX matter on lossy relay links, and the same codec runs host-side so the
  CLI can drive tests. libopus 1.5.2 is vendored in `vendor/opus` and built by `sys/opus-sys` with CMake (bindings from
  `just bindgen`); for Android it is presented as a plain Linux cross compile so our clang wrapper is used, with
  `CMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY` because that wrapper links with `-nodefaultlibs`.
  **Wire format:** one 20 ms Opus packet (48 kHz mono) per QUIC datagram, never retransmitted. The receiver's jitter
  buffer pre-buffers 60 ms, rebuilds a lost packet from the next packet's FEC, else conceals, caps latency at 200 ms
  and re-buffers after 200 ms of loss. **Android:** AAudio in voice-communication mode (platform AEC/NS/AGC), with the
  realtime callbacks only moving samples through `rtrb` rings and a 20 ms tokio task doing the codec work;
  `AudioManager` is switched to `MODE_IN_COMMUNICATION` with the speaker on. **CLI:** `--video clip.mp4` now also
  sends the clip's AAC audio (decoded once by symphonia to 48 kHz mono, resampled if needed) and `--record out.mp4`
  writes the peer's H.264 and Opus as received into one fragmented MP4 (`mp4-atom`), so an abrupt hang-up still
  leaves a playable file. The `mp4` crate is gone; `mp4-atom` both reads and writes.
- **2026-09-22**: audio first call: voice worked both ways, then a later call had none — AAudio returned `Disconnected`
  opening the speaker, because entering call mode re-routes audio and kills streams opened as it happens.
  **Stream lifecycle, per Oboe's disconnect note:** a disconnect is a normal event (route change, headphones,
  Bluetooth), the error callback may not stop/close/reopen the stream, and both streams are closed and reopened from
  another thread. Our error callback only records it in `AudioHealth`; the UI's 1 s timer reopens, and the fresh rings
  are handed to the running pump so the codecs and call are untouched. A failed open at call start is the same path,
  retried each second. **A stream disconnected before it ever ran gets no error callback** (seen on the S24: the
  capture stream came back `Disconnected` straight from `open`, while playback was fine, so the phone heard the peer
  but sent silence), so `needs_reopen` polls both stream states as well as the callback's flag, and `open` refuses
  streams that are already disconnected. Per-second `voice` log lines carry mic/speaker sample counts, since a silent
  microphone is otherwise invisible without adb. To do: `setCommunicationDevice` (API 31+) instead of the deprecated `setSpeakerphoneOn`, needed
  for BLE headsets.
- **2026-09-22**: step 6 (call service + Java log shim) done; tested by backgrounding a live call. `UplinkCallService` is a
  foreground service with the camera and microphone types, which is what lets a call keep using them once the activity
  is hidden; the app starts it when a call connects and stops it when the call ends or the activity exits, and the
  camera is no longer torn down on Pause while a call is up. `axml` gained a `service=` element (attribute ids and
  `ServiceInfo.FOREGROUND_SERVICE_TYPE_*` values read from `android.jar`, not guessed), plus the
  `FOREGROUND_SERVICE{,_CAMERA,_MICROPHONE}` and `POST_NOTIFICATIONS` permissions. **Java log shim:**
  `UplinkActivity.log` writes to logcat and, through a new `nativeLog` native, into this run's log file under the
  `java` target, so Java-side failures are visible without adb. Its handle is a Java `static` (one activity per
  process, cleared before the native handle is released) so the service can log too. To do: an incoming call while the
  app is backgrounded cannot start a camera/microphone service (Android's while-in-use rule); that needs a
  full-screen-intent notification and starting the service once the user answers.
- **2026-09-22**: recordings play in VLC. A valid-but-rejected file needs: `stco` present (even empty) in every
  `stbl`, real durations in `mvhd`/`tkhd`/`mdhd` (patched in when the recording closes, since fragments alone leave
  them zero), and the `iso5`/`dash`/`msdh` brands.
- **2026-09-22**: **telemetry revised**. Stats are **not** exchanged with the peer (no extra network overhead). Each
  device logs its own periodic call stats. A later diagnostic-report button packages the logs into a shareable form.
  Step 4c = periodic call stats in the log.
- **2026-09-22**: **step 7 UI design locked**.
  - **Color:** one accent, `beacon #58B6FF`; green `#32C97A` and red `#FF5252` are reserved for answer/end and
    nothing else. Dark call ground `ink #070A0F`, `surface #101720`, `raised #18212C`, text `#E9F0F7`, muted
    `#93A3B5`; neutrals biased blue. Each token maps to a Material 3 role (primary, surface-container,
    on-surface-variant, …), so M3 naming applies without M3's default palette. Chrome over video sits on a
    top-to-transparent scrim, never a panel.
  - **Type:** Outfit (names, headings), Public Sans (prose), IBM Plex Mono with tabular figures for the call timer
    and key fingerprints (grouped in fours). Sizes follow the M3 roles.
  - **Motion,** and nothing else moves: answer = self-view shrinks full-frame → top corner, 620 ms
    `cubic-bezier(.2,.8,.3,1)`, peer fades up behind; preview swap 420 ms on the same path; one ring pulse while
    ringing; controls fade after 4 s idle. Reduced motion makes the answer a cut.
  - **Layout is written start/end, never left/right, from the first line**: Skia shapes Arabic for free, but Slint
    1.18 does not mirror layouts, so one direction flag drives rows, icons and alignment. Keys, fingerprints and
    the timer stay LTR inside Arabic text; numerals follow the language.
  - **Accessibility:** 48dp touch targets with TalkBack labels, 4.5:1 text contrast, state never by color alone
    (the mute key changes fill, not just hue), system text size respected.
  - **Phases:** 7a = contacts (add/rename/remove, initials avatars), own QR + scanning, incoming/outgoing/in-call,
    mic/speaker/flip/end, answer transition, preview swap. 7b = PiP (Android `enterPictureInPictureMode` over JNI;
    Slint just draws into the smaller surface), lock-screen incoming call (full-screen-intent notification, service
    started on answer — the step 6 follow-up), settings (theme, language, quality), diagnostics report, key from a
    saved image. 7c = safety-code verification and key-change warning, identity rotation, photo avatars, history.
  - **Spikes before their phase:** PiP, and QR scanning (camera frames decoded in Rust) + reading a key from a
    saved image (needs a file picker through the activity).
  - **Deep links (later):** a scanned code should open uplink directly rather than a raw key — an `uplink://` URI
    plus an https link carrying the key, with the activity registered for both (Android App Links), so a key sent
    over chat is one tap. Decided 2026-09-22; not scheduled yet.
- **2026-09-22**: step 7a done: contacts, QR and the call screens, tested on the S24. The UI follows the locked
  design, with the self-view animating into its corner on answer, tap to swap, mute / speaker / flip and an mm:ss
  timer. **Display faces to do:** Outfit/Public Sans/IBM Plex Mono get vendored into the repo — Slint registers a
  font from the markup (`import "./Outfit-Regular.ttf";`), so no build script is involved; until then the system
  face carries the spec's sizes and weights and `monospace` carries keys and the timer.
  **QR:** `qrcode` encodes *and draws* (its `image` feature, `image` with default features off = buffer types, no
  codecs), `rqrr` decodes; the only pixel code of ours is widening greyscale to RGB for Slint. A hand-rolled blit
  cost a session: `Matrix::dark` indexed `y * size + x` without bounds-checking `x`, so the right quiet column
  wrapped into the next row and sheared every row — the fix is the test that decodes our own rendered code.
  **Scanning** reads only the luma plane of a second CPU-readable camera stream (YUV_420_888, back camera): luma is
  already greyscale, so no YUV→RGB anywhere — the preview stays GPU-only through `samplerExternalOES`. The reader's
  callback copies the frame and hands it to `spawn_blocking`, one decode at a time; decoding *in* the callback
  deadlocked teardown. Keys can also be read from a saved image: Android's `BitmapFactory` decodes any format
  (subsampled to 1600px) and returns ARGB, which we reduce to luma.
- **2026-09-23**: the build was matched to the locked design by measurement, not by eye, and the method is written up in
  [docs/ref/design-fidelity.md](ref/design-fidelity.md) — read it before editing `ui.rs`. The mockup is vendored at
  `docs/design/uplink-call-ui.html`; `just design-measure` / `design-shot` measure and screenshot it in headless
  Chromium, `just preview` dumps Slint's item tree beside each render, `just ui-diff` compares the two by bands of ink.
  The mockup's 248px frame stands for a **360dp** phone (×1.4516). Nine divergences an earlier session had marked
  "closed" with reasons were withdrawn: the design is the specification. Outfit, Public Sans and IBM Plex Mono are
  vendored in `assets/` (with `assets/icons/`), reached by one `#[include_path]`; `font-family: "monospace"` never
  resolved, and the design's font sizes are fractional. The app has **light and dark**, following the system unless
  forced from Settings; a call stays dark in either.
- **2026-09-23**: **shell, profile and privacy designed**.
  This **supersedes** "contacts are iroh keys only", "avatars: initials on a generated colour, no image pipeline, no
  storage", and the People screen's "Your key" footer row.
  - **Three permanent tabs: People, Connect, Settings.** People is contacts only — favourites first, last-called as the
    second line, the row opens the contact (favourite, rename, remove) and a trailing pill calls. Connect is your code
    plus scan/choose-an-image, and is the only place a person is added. Your identity is not a contact and leaves People.
  - **The mark is the "u-link":** a lowercase *u* whose two ends are the peers it joins. Two colours, no gradient, a
    handful of SVG paths, and it survives the adaptive-icon mask.
  - **First run:** splash (dark in either theme, `P2P · E2EE`) → permissions → *Who are you?* (name, optional picture) →
    *This is your identity* → People. **Camera, microphone and notifications are all required**; the gate explains why
    before Android's dialogs, and a blocked state lists what is missing with a button to this app's own settings page
    (`ACTION_APPLICATION_DETAILS_SETTINGS`), re-checking on resume. `POST_NOTIFICATIONS` is only a runtime permission
    from API 33, so on API 30–32 the gate counts it granted and shows two rows.
  - **To do — a resource table.** `android:icon` and the Android 12+ system splash are resource references, and the
    SDK's `aapt2` is an x86-64 binary that cannot run on this arm64 host. So `resources.arsc` has to be written the way
    `axml` writes binary XML: string pools we already have, a package chunk, type-spec and type chunks, simple entries
    for the icon and map entries for the splash theme. minSdk 30 means **adaptive icons are always available**, so
    `mipmap-anydpi-v26` alone is enough and no legacy PNG mipmaps are needed. Until it exists the launcher shows
    Android's placeholder; the in-app splash, the permission gate and the tab shell need none of it.
- **2026-09-23**: **noted while designing the shell, all still to do.**
  - **Your own identity must never become a contact.** Refuse it wherever a key is accepted — scan, image, paste — with
    a plain message rather than a silent no-op. Calling yourself is not a feature.
  - **Errors belong inline** where the thing went wrong; a modal only when the user has to decide something. A toast
    that disappears is not an error report.
  - **Nothing destructive happens on one tap.** Clearing the call log and removing a contact both
    need confirming — a tap can be an accident, and a contact is a key that may be gone for good.
    Contacts also want **batch selection**, so clearing out several is one confirmation rather than
    one per row.
  - **A dialling call is still a call.** The camera preview should be up while it rings, all the
    controls should be there and working rather than present-but-inert, and answering should play
    the locked transition — the self-view folding from full frame into its corner. A call that
    times out should **stay on the call screen** long enough to say it did not connect, the way
    every phone does, instead of vanishing back to the list. "Calling…" wants its dots animated.
  - **Tapping the self-view hides it instead of swapping it.** It is meant to trade places with
    the remote picture, which is what every other call app does with that tap.
  - **A call should say what it is riding on.** A small marker on the call screen for direct
    versus relayed, because the two behave nothing alike — a relayed call between two carrier
    NATs has run at 618 ms average with 12-second excursions, while the same pair direct was
    113 ms. The user cannot fix the route, but "relayed" explains what they are seeing.
  - **Network telemetry, on the screen rather than only in the log.** Throughput each way, and a
    banner when a call is struggling that says **whose side** it is: our own send rate collapsing
    is a different sentence from frames not arriving. The counters already exist in
    `uplink_core::telemetry`; what is missing is saying it to the person in the call.
  - **Bitrate has to follow the path.** RTT climbing to seconds with no loss and no congestion
    event is a queue filling somewhere, and the answer is to send less, not to keep filling it.
    The encoder's bitrate should come down when RTT rises and recover when it falls; the frame
    deadline and the in-flight window want revisiting at the same time, because at 12 s RTT every
    frame ages out and the window jams.
  - **A call attempt needs a sound and an end.** Ringback while an outgoing call is ringing, a
    ringtone for an incoming one, and a **timeout on establishing an outgoing call** — dialing a
    key nobody is listening on currently waits until the user gives up. The tone routes through the
    existing AAudio path, and stops the moment the call connects or ends.
  - **A call must survive leaving the call screen** — this is a stack decision, not a feature, and
    is open. What is settled: a call already keeps running in the background on a foreground
    service. What is not:
    - **Picture-in-picture** is the platform's answer to "use the phone while calling". It needs
      `supportsPictureInPicture`, `enterPictureInPictureMode`, and `RemoteAction`s for the
      controls inside the small window. It costs nothing in permissions and works when the user
      leaves uplink entirely, which an in-app floating frame cannot. The UI has to have a PiP
      layout — the remote picture and nothing else — driven off the size Slint is given.
    - **`Notification.CallStyle`** (API 31+) is what makes the ongoing-call notification look and
      behave like a call, with hang up in the shade. Mic and speaker are extra actions on it. The
      call service already posts a notification; this is a change of style, not of architecture.
    - **A self-managed `ConnectionService`** is the real question. It is what makes Android treat
      ours as a call: audio focus and routing handled by the system, Bluetooth headset buttons,
      and the right behaviour when a cellular call arrives mid-call. It costs a `PhoneAccount`,
      the `MANAGE_OWN_CALLS` permission and a meaningful amount of Java. Self-managed calls do
      **not** appear in the system call log and do not use the system's in-call UI. Deciding this
      also decides whether we keep driving `MODE_IN_COMMUNICATION` ourselves.
  - **Strings get translated with Slint's own scheme**, which is gettext (`@tr()` → `.pot` → `.po`), not FTL. Bionic has
    no gettext, so use **bundled** translations: the compiler takes `SLINT_BUNDLE_TRANSLATIONS=<dir>` as an environment
    variable, so the `slint!` macro can bundle without a build script, and `slint::select_bundled_translation` switches
    at runtime — which is what makes the Settings "Language" row real. Extraction wants `slint-tr-extractor`
    (`cargo install`), so ask before that step.
- **2026-09-23**: **the resource table is done and the icon is on the phone.** `tools/axml` became
  `tools/android-res`, which compiles real XML under `android/` — manifest, `values.xml`, an adaptive icon — into
  binary XML and `resources.arsc`, and writes the APK zip itself. Method notes and the format's two hard rules are in
  [docs/ref/android-res.md](ref/android-res.md). Three things worth carrying: **`zipalign` is x86-64 like `aapt2`**, and
  API 30+ refuses an APK whose `resources.arsc` is compressed or unaligned, so the zip is ours (stored + 4-byte aligned
  via a `0xd935` extra field, everything else deflated); **`apkanalyzer` is Java**, so it verifies output here without a
  device; and framework ids are **generated** from `android.jar` (`just android-table`) after a hand-copied
  `Theme.DeviceDefault.DayNight` turned out to be wrong — a wrong id fails silently, which is the worst kind.
  The system splash needs no `values-v31`: Android 12+ composes it from `android:icon` and the theme's
  `windowBackground`, so pointing those at the mark and `@color/ground` makes it match the in-app splash, and on API 30
  the same attribute paints the window before the first frame.
- **2026-09-23**: **the system bars and the keyboard.** The white status bar was not an inset bug —
  the insets were already applied. Under the enforced edge-to-edge of recent target levels the bars
  have no background of their own, so what shows through them is our own ground, and Android was
  painting white icons on it in the light theme. The app now tells it which way to go
  (`WindowInsetsController.setSystemBarsAppearance`, via `UplinkActivity.setLightSystemBars`), driven
  by a `changed` handler on the window: a bare page follows the theme, while a call, a dimmed sheet
  and the log are dark whatever the theme says.
  `adjustResize` does **not** survive edge-to-edge — the window is not resized; the keyboard arrives
  as an inset, which Slint's Android backend already dispatches as `virtual-keyboard-size`. So the
  contact screen puts its body in a `ScrollView`, which is what Slint looks for when it brings a
  focused field back into view, and the naming sheet centres itself in what the keyboard leaves
  rather than in the window. Separately: a `ScrollView` does **not** pan under a finger unless
  `mouse-drag-pan-enabled` is set — every list in the app was drag-only on its scrollbar.
- **2026-09-23**: **an identity is shared as its code, not as its key.** "Share my identity" now
  sends a picture — the code with the uplink mark on a plate in the middle and *Scan to connect*
  under it — the way a payment app does, because a picture is both what people send each other and
  something the other side can already open with "Choose an image".
  The mark in the middle is why every code is now drawn at error correction **H**: level H spends
  about 30% of the code on recovery and the mark covers a fifth of its width, which is a
  twenty-fifth of its area. `qr::Matrix::logo` owns that number, a test punches exactly that hole
  and decodes the result, and both the screen and the shared card are drawn from it.
  **The card is drawn in Rust**, in `uplink_core::card`: `ab_glyph` rasterises the one line of
  text and `image` reads the mark and writes the PNG, which is three small crates against a
  platform-specific drawing that could not be tested. Now the thing that actually matters — that
  the finished picture still decodes to the key, mark and caption and all — is a test, and
  `just preview` writes the card out beside the screens. Java keeps only what has to be Java:
  sharing a file needs a `content://` URI at this target level, so a minimal read-only
  `ContentProvider` (`UplinkFiles`) serves the one directory of the data dir that Rust writes to.
  `imageproc` was the obvious candidate and was passed over: `draw_text_mut` would have saved
  about twenty lines and brought rayon, rand and the rest of an image-processing library with it.
  The mark's art is **not centred in its own 48-unit viewBox** (it runs y 4.4–38.75), so the badge
  is a separate render from `mark.py` with the art's bounding box as the viewBox; a square crop of
  the viewBox would hang the mark high on its plate.
- **2026-09-23**: **a second phone, and what it cost to find out why.** Everything below came out
  of one device — an Android 12 Huawei on HiSilicon — and none of it reproduced on the Snapdragon
  phone the app was written on.
  - **Notifications**: `POST_NOTIFICATIONS` is Android 13. On 12 the name is not a field of
    `Manifest.permission` at all, so looking it up **throws** rather than returning false, and the
    app read its own notifications as refused for good however often they were granted. Below 13
    there is nothing to ask: `NotificationManager.areNotificationsEnabled` is the whole answer.
  - **`low-latency` is a hint, and optional.** `OMX.hisi.video.decoder.avc` refuses a format that
    carries it; `c2.qti.avc.decoder` accepts it. It was also being set on the encoder, where it
    means nothing. Now decoder-only, and a refusal retries without it.
  - **The capture request template is not cosmetic.** It sets `CONTROL_CAPTURE_INTENT`, which is
    how the camera HAL learns a stream feeds an encoder. Handed the encoder's surface under
    `TEMPLATE_PREVIEW`, the HiSilicon device configured the session, delivered no frames at all,
    and then faulted with `ERROR_CAMERA_DEVICE`. `TEMPLATE_RECORD` while a call is up.
  - **A decoder's output is not all pictures.** Codec configuration and end-of-stream come back
    as output buffers too, and either can carry no bytes; releasing one *with render* draws
    nothing onto the surface, which on screen is a black frame. `OMX.hisi.*` emits them and
    `c2.qti.*` does not, so it looked like a network fault on one phone only.
  - **The method that found all of it** was making failures legible, and that is worth more than
    the fixes. `MediaError`'s Display is `{:?}` of itself, so a bare `?` wrote "ErrorUnknown" and
    nothing else; the variants now carry the call name, as `Camera` and `Egl` already did, and
    dropping `#[from]` made the compiler point at all fifteen places that were discarding it.
    `jni` reports every throw as the word "JavaException" and leaves the throwable pending, so
    `with_activity` lifts its `toString()` across before clearing it. The log rolls by size
    (ten files of ten megabytes) and Settings sends it as a `tar.gz` — a log a tester on another
    continent cannot send is a log nobody reads.
  - **Relay versus direct is the open performance question.** The same two phones ran direct at
    113 ms and, an hour later, relayed at 618 ms average with 12-second excursions at zero loss
    and zero congestion events. The peer's carrier NAT is symmetric (a different external port in every
    sample), and on WiFi this side advertises no public IPv4 at all — so IPv4 punching cannot
    work. Both ends have global IPv6, which should sidestep it and is not being used; a router's
    inbound IPv6 filter is the first suspect.
