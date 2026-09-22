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

- Identity is an iroh keypair per device. Contacts are **iroh keys only**; names are local nicknames. Contacts are added
  by QR scan or pasted key. No accounts, phone numbers or servers of our own.
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
7. **Product UI**: QR identity/scan, contacts, incoming/outgoing/in-call screens, verification.
8. Security to-dos above; then iOS (CI) and web.

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
