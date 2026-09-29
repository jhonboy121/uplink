# uplink

Peer-to-peer, end-to-end encrypted voice and video calls for Android, in Rust. No accounts, phone
numbers or servers of its own: every device is an [iroh](https://iroh.computer) key, calls are dialled by
key, and a relay carries a call only when no direct path can be found.

Android comes first. The same core runs on the desktop as a terminal client (`uplink-cli`), which can
call a phone.

## Read this first

- **This project is AI-assisted.** Much of the code, documentation and commit history was written with
  an AI coding assistant, then reviewed and tested by a human on real devices. Expect the mistakes that
  come with that.
- **The security has not been formally verified or audited.** The encryption is iroh's QUIC/TLS 1.3
  between keys, and nothing in this repository has been reviewed by a security professional. Do not
  rely on it where interception or impersonation would put anyone at risk.
- **Use at your own risk.** It is experimental software, provided as is, without warranty of any kind.
  It may drop calls, lose data or behave in ways the documentation does not describe. The authors
  are not liable for anything that comes of using it.
- Recording a call without the other person's consent is against the law in many places. Know yours.

## Layout

| Path | What it is |
|---|---|
| `crates/uplink-core` | Platform-agnostic core: identity, contacts, iroh endpoint, call signalling, media framing, relays |
| `crates/uplink` | The Android app: Slint UI, camera, codecs, audio, JNI |
| `crates/uplink-android` | Android platform glue and the Java shim (`java/dev/uplink`) |
| `crates/uplink-cli` | Terminal client: identity, contacts, and a call TUI |
| `tools/` | Android resource compiler, binding generator, headless UI preview, dev helpers |
| `external/` | Submodules: [opusorus](https://github.com/jhonboy121/opusorus) (Opus), [h264](https://github.com/jhonboy121/h264) (the CLI's H.264), and patched forks of [ndk](https://github.com/jhonboy121/ndk/tree/uplink), [ndk-context](https://github.com/jhonboy121/ndk-context/tree/uplink) and [net-tools](https://github.com/jhonboy121/net-tools/tree/uplink) (netwatch) |
| `docs/plan.md` | Decisions and their history; `docs/ref/` has API notes |

## Requirements

- Rust (edition 2024) with the Android target: `rustup target add aarch64-linux-android`
- [`just`](https://github.com/casey/just)
- Android NDK r30 at `~/android/ndk`, or set `ANDROID_NDK_HOME`
- Android SDK at `~/android/sdk`, or set `ANDROID_HOME`, with `build-tools;37.0.0` and
  `platforms;android-37.0`
- JDK 17 (`JAVA_HOME`), for `javac`, `keytool`, d8/R8 and apksigner
- `clang`, `lld` and `llvm-strip` on the host (the NDK's own clang is not used; see `tools/android-clang`)
- `adb` and an arm64 phone running Android 12 (API 31) or newer
- For formatting only: [`taplo`](https://taplo.tamasfe.dev) and google-java-format
  (`GOOGLE_JAVA_FORMAT` points at its jar)

## Setup

```sh
git clone --recursive https://github.com/jhonboy121/uplink.git
cd uplink
# already cloned without --recursive:
git submodule update --init
```

**Your relay.** uplink prefers its own self-hosted [iroh-relay](https://github.com/n0-computer/iroh),
then n0's public relays, then any you add in the app. The URL of the self-hosted one is compiled in and
never committed. Set it in `.cargo/config.local.toml`, which git ignores:

```sh
cp .cargo/config.local.toml.example .cargo/config.local.toml
# then set UPLINK_RELAY = "https://relay.your-domain.example"
```

Without the file, builds use a placeholder example domain in its place, leaving n0's relays and any
you add.

## Build and run on Android

```sh
just apk                  # debug APK, written to ~/android/out/uplink.apk
just run                  # build, install over adb and launch
just logcat               # follow the app's log (Java and native share the tag `uplink`)
just profile=release apk  # release build: optimised, R8-shrunk, not debuggable
```

The APK is signed with `~/android/debug.keystore`, which is created on the first build. Release and debug
builds share that key, so either installs over the other without losing data.

## The terminal client

```sh
just cli --dir /tmp/a id                 # print this identity's key
just cli --dir /tmp/a add alice <key>    # save a contact
just cli --dir /tmp/a contacts
just cli --dir /tmp/a                    # the call TUI
just cli --video clip.mp4 --record target/rec.mp4
```

Two clients with different `--dir`s can call each other, or one can call a phone. Without `--video` the
camera is a test pattern. The log goes to `target/cli.log` (`UPLINK_CLI_LOG`).

## Development

```sh
just test        # uplink-core unit and integration tests, on the host over loopback
just clippy      # warnings are errors
just fmt         # cargo fmt, taplo, google-java-format
just fmt-check
just preview     # render every screen to target/ui-preview, no phone needed
just             # list every recipe
```

## Self-hosting a relay

A standard `iroh-relay` behind TLS works. To keep it private, allowlist your devices' endpoint ids in
its config (`access.allowlist = ["<hex endpoint id>", ...]`) and restart it; the relay reads its config
only at startup. Keep its metrics port (9090 by default) off the public internet.

## License

uplink's own code is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you say otherwise, any contribution you submit for inclusion is dual-licensed
the same way, without additional terms.

It is built on third-party software under its own licenses, listed in [THIRD-PARTY.md](THIRD-PARTY.md).
The UI is [Slint](https://slint.dev), used under its royalty-free license for desktop, mobile and web
applications, whose attribution is this badge:

[![Made with Slint](https://raw.githubusercontent.com/slint-ui/slint/master/logo/MadeWithSlint-logo-whitebg.png)](https://slint.dev)
