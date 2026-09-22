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
  - **Post-quantum:** check whether iroh/rustls can use hybrid X25519+ML-KEM (Signal-style "record now, decrypt later" protection).
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
