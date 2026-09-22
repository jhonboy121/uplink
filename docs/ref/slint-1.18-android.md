# Slint 1.18 on Android — reference (verified as we go)

Slint **1.18.1**. Source: `~/.cargo/registry/src/*/slint-1.18.1/`,
`i-slint-backend-android-activity-1.18.1/`, `i-slint-compiler-1.18.1/`, `i-slint-core-1.18.1/`.
License: GPL-3.0-only OR Slint Royalty-free 2.0 OR Slint Software 3.0.

## Cargo

```toml
slint = { version = "=1.18.1", default-features = false,
          features = ["std", "compat-1-2", "backend-android-activity-06"] }
jni = "0.22"          # same version Slint's backend uses (see jni-0.22.md)
```
- `backend-android-activity-06` = android-activity 0.6 **NativeActivity** + ndk 0.9. There's also
  `-05`, and a `game-activity` feature on the backend crate.
- `[lib] crate-type = ["cdylib"]`, entry point `#[unsafe(no_mangle)] fn android_main(app: slint::android::AndroidApp)`.
- **Skia is always used on Android.** `androidwindowadapter.rs` hard-codes `SkiaRenderer`, so
  `renderer-femtovg` has no effect there. The `unstable-wgpu-29/30` features switch Skia to a wgpu
  backend (`SkiaRenderer::default_wgpu_30`).

## Build requirements (on-phone setup)

- **skia-bindings 0.153.3** downloads prebuilt
  `skia-binaries-b7f043e0b1e2a850e702-aarch64-linux-android-ganesh-gl-jpegd-jpege-pdf-vulkan.tar.gz`
  (7.1 MB → `libskia.a` 23.5 MB). It's cached at `~/android/cache/`. Point
  `SKIA_BINARIES_URL = "file:///path/to/skia-binaries-{key}.tar.gz"` at it (`file://` supported).
  Needs `ANDROID_NDK` set, and hardcodes the `linux-x86_64` prebuilt dir name (our NDK extraction matches).
  Links `c++_static` + `c++abi` from the NDK sysroot.
- **Java helper:** the backend's build.rs compiles `java/SlintAndroidJavaHelper.java` with `javac` (source/target 8),
  then `d8`, via the `android_build` crate. Needs `JAVA_HOME` (JDK 17 OK; 21 fails with old build-tools) and
  `ANDROID_HOME`/`ANDROID_SDK_ROOT`. It picks the highest platform (`android-37.0`); override with `ANDROID_PLATFORM`.
  The output `classes.dex` (~24 KB) is embedded and loaded at runtime with `InMemoryDexClassLoader`,
  so the APK needs **no** `classes.dex` and `hasCode=false` is fine.
- All env vars go in the project's `.cargo/config.toml` `[env]` (Claude's shell doesn't source `.bashrc`).

## Init and lifecycle ✅

```rust
slint::android::init(app)?;                                  // simple
slint::android::init_with_event_listener(app, |ev: &PollEvent| { … })?;  // Fn + 'static, main thread
use slint::android::android_activity::{MainEvent, PollEvent};
// PollEvent::Main(MainEvent::{Start, Resume{..}, Pause, Stop, InitWindow{..}, TerminateWindow{..},
//                             GainedFocus, LostFocus, ConfigChanged{..}, LowMemory, Destroy, ..})
```
- Grab `app.internal_data_path()`, `app.vm_as_ptr()` and `app.activity_as_ptr()` **before** moving `app` into init.
- To update UI from the listener, store `slint::Weak<App>` in a `thread_local!` (the listener runs on the UI thread).

## Window properties useful on mobile ✅

Built-ins (`i-slint-compiler/builtin_elements.rs`), all `out` on `Window`:
- `safe-area-insets: Edges` → `.top/.bottom/.left/.right` (added in 1.15)
- `virtual-keyboard-position: Point`, `virtual-keyboard-size: Size`

```slint
VerticalBox {
    padding-top: root.safe-area-insets.top;
    padding-bottom: max(root.safe-area-insets.bottom, root.virtual-keyboard-size.height);
}
```

## Backend capabilities (from `lib.rs` / `javahelper.rs`)

Clipboard get/set, IME (`set_imm_data`, `show/hide_keyboard`, cursor handles, action menu),
dark/light `color_scheme`, `accent_color`, `font_scale`, safe area/view rect, `finish_activity`,
long-press interval, `invoke_from_event_loop` / event-loop proxy.

## Open questions (spike 2/3)

- [x] IME shows and commits text on the S24; layout follows `virtual-keyboard-size` (spike 2 ✅)
- [x] Surface loss on background → foreground recovers (spike 2 ✅). Panic hook + ApplicationExitInfo (CRASH_NATIVE tombstone) work without adb ✅
- [x] Video frames (spike 3 ✅, S24): Camera2 → AImageReader PRIVATE+GPU_SAMPLED_IMAGE → EGLImage → external-OES blit in `BeforeRendering` → `BorrowedOpenGLTextureBuilder` (default TopLeft origin, ping-pong 2 textures, `set_frame` inside the notifier). 30 fps camera + blit, process CPU 10–20% of one core (debug build). Must save/restore Skia GL state (FBOs, viewport, program, tex/sampler bindings on unit 0, VAO, PBO, color mask, scissor/blend/depth/stencil/cull).
- [ ] APK size impact of Skia. Debug so far: clean build of ~520 crates 1m49s on the S24 (8 cores, nice 10);
      `.so` 85 MB → 46 MB after `strip --strip-debug`; APK 13 MB. A release measurement is still needed.
