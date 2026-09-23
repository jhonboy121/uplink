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

## Layout rules we got wrong once ✅

The official `slint` plugin (marketplace `slint-ui/ai-plugins`) ships a skill and a docs MCP
server — use them before writing markup. What the first pass of our UI got wrong:

- **Overlay vs. row.** A bottom bar placed as an absolutely positioned child *covers* content.
  Put the bar in the root `VerticalLayout` so it takes space; declare true overlays (in-call
  screen, log) after it, since later siblings draw on top.
- **`VerticalBox`/`HorizontalBox`** over raw layouts with hand-set padding, on one spacing scale
  (8px, halved to 4px). `padding`/`spacing` only do anything on *layout* elements: on a
  `Rectangle` they compile to a deprecation warning and are ignored.
- **Stretch is what creates voids.** A `ScrollView` or spacer with `vertical-stretch: 1` and
  nothing in it leaves a black expanse; align content to `start` and let one stretched
  `Rectangle {}` be the spacer.
- **`rem` for font sizes** (`default-font-size` on the Window sets the base), not a different px
  per label. `em` does not exist.
- **`Palette.color-scheme`** decides how std-widgets paint themselves; set it (we force dark) or
  stock buttons and line edits will fight the surfaces around them.
- **Safe-area insets belong inside the component** that draws to the edge: pass the inset in and
  add it to that element's own layout padding.
- Outside a layout an element with implicit size is *centered*; set `x: 0; y: 0` for top-left.
- **Fonts are vendored, not build-scripted.** `import "./Outfit-Regular.ttf";` at the top of the markup registers
  the family for `font-family`; paths resolve relative to the file holding the `slint!` macro, as `@image-url` does.
- **Icons are SVG assets** via `Image { source: @image-url("../icons/mic.svg"); colorize: <brush>; }` — Unicode
  glyphs depend on font coverage and render blank. SVG needs `i-slint-core`'s `svg` feature; the `slint` facade
  exposes no such feature, so depend on `i-slint-core` directly for it.
- **Render before declaring UI done:** `just preview` draws every screen with the software renderer into
  `target/ui-preview/`. It caught blank glyphs, a bar covering content, and buttons eating all the slack — none of
  which the compiler sees.
