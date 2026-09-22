# Patched `ndk` / `ndk-sys` (one copy for the whole graph)

Upstream: https://github.com/rust-mobile/ndk (master `244337b4b21297f9fc15a6451fa465cbdf6effb8`, 2026-07-22). Our changes are two commits on it, branch `uplink` of the fork
https://github.com/jhonboy121/ndk, the submodule at `external/ndk/`. At that commit
`ndk` = 0.9.0, `ndk-sys` = 0.6.0+11769913 (bindings built from an older NDK CI build, and no camera link).

## Why

- We want Camera2 + MediaCodec + AImageReader/AHardwareBuffer bindings **generated**, not hand-typed.
- Upstream bindings come from older headers and never link `camera2ndk`.
- Slint → android-activity → ndk/ndk-sys must all use the **same** `ndk-sys`, or you get
  type mismatches (`ALooper` from one copy vs `ALooper_addFd` from another).

## Local changes

- `tools/ndk-bindgen/` (workspace member): Rust generator (bindgen `=0.72.1` as a library) that replaces `generate_bindings.sh`.
  It uses upstream's exact flags (blocklist, 40 newtype enums, `--rust-target 1.60`) and reads
  headers from the local NDK sysroot (`$ANDROID_NDK_HOME` or `~/android/ndk`) instead of a Google CI artifact.
  Run: `just bindgen`. It also generates our `sys/ndk-gl-sys` (EGL/GLES3, edition 2024).
  (needs Alpine `clang22-libclang` + `clang22-headers`).
  bindgen 0.71 gotcha: `RustTarget::stable(60, 0)` returns `Result<_, InvalidRustTarget>` without `Debug`,
  so use `.ok().expect(..)`.
  **musl host gotcha:** Rust's musl binaries are fully static, and `dlopen` fails with "Dynamic loading not supported".
  So bindgen runs with `default-features = false, features = ["logging", "prettyplease"]` (no `runtime`), which
  makes clang-sys link `libclang.so` at build time. The workspace `.cargo/config.toml` sets
  `-C target-feature=-crt-static` for `aarch64-unknown-linux-musl` plus `LIBCLANG_PATH=/usr/lib/llvm22/lib`.
- **libclang 22 gotcha:** bindgen 0.71.1 (upstream's version) with libclang 22 made `ANativeActivityCallbacks` opaque
  (`_address: u8`, all 16 fields gone), which breaks android-activity. LLVM 22 removed `ElaboratedType`, and bindgen
  0.72.1 fixed the interaction (rust-bindgen#3278). So the generator pins `=0.72.1`, the smallest bump with the fix
  (0.73 also merges extern blocks, which reshapes the output). Sanity check: upstream's bindings have **0** opaque
  structs, so `grep -B2 'pub _address: u8' ffi_aarch64.rs` should come back empty.
- **NDK r30 gotcha:** `sys/cdefs.h` errors with "Unversioned target triples are not supported!". Upstream's
  unversioned triple exposed every API. The generator reproduces that with `--target=<triple>10000` (`__ANDROID_API_FUTURE__`; with 21, functions such as `android_get_device_api_level` and `strto*_l` turn into static-inline fallbacks that bindgen skips)
  plus `-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__`, which makes `__BIONIC_AVAILABILITY_GUARD` always 1, so all
  declarations are visible. The `ndk` crate still gates APIs through its `api-level-*` features.
- Rust 1.98 warns `suspicious_runtime_symbol_definitions` for memcmp/memcpy/memmove/memset/strlen (`c_ulong` vs `usize`).
  Fixed by blocklisting those five functions in the generator (unused by `ndk`); no `allow`.
  It's harmless on 64-bit.
- `ndk-sys` version `0.6.0+r30.16248370`. The `+…` is build metadata, so it still satisfies `0.6.0` requirements.
- `ndk-sys` feature `camera` → `#[link(name = "camera2ndk")]`.

## Consuming it

```toml
[dependencies]
ndk-sys = { version = "0.6", features = ["camera", "media"] }

[patch.crates-io]
ndk = { path = "external/ndk/ndk" }
ndk-sys = { path = "external/ndk/ndk-sys" }
```

**Lockfile gotcha:** if `Cargo.lock` was created before the patch, cargo keeps the old registry
`ndk-sys` for some dependents, so there are two copies. Fix with
`cargo update --offline -p 'ndk-sys@0.6.0+11769913'`, then verify with
`cargo tree -i 'ndk-sys@0.6.0+r30.16248370' --target aarch64-linux-android` (everyone should be listed,
and the old version should be gone). The `0.5.0` entry from the optional android-activity 0.5 path is inert.

## Status

- [x] Patch unifies the graph (android-activity, ndk, Slint backend → our ndk-sys)
- [x] Slint stack type-checks against the patched crates (still upstream's bindings), 26.75s
- [x] r30 regeneration runs for all 4 arches (+38k/−35k lines vs upstream)
- [x] r30 regeneration diff reviewed (aarch64: 5496 → 5925 items, +471 / −42; every removal explained below)
- [x] Slint stack (spike 2) type-checks against r30 bindings (11s)
- [x] `cargo check -p ndk --all-features --target aarch64-linux-android` passes (35s)
- [x] 0 opaque structs on all 4 arches

## Upstream `ndk` fixes carried on our branch

- `ndk/src/data_space.rs`: older NDK headers declared `STANDARD_BT2020_CONSTANT_LUMINANCE` without the `ADATASPACE_`
  prefix (a header typo), and upstream `ndk` relied on that spelling. r30 fixed the header, so we use
  `ADATASPACE_STANDARD_BT2020_CONSTANT_LUMINANCE`. Worth sending upstream.

## Verifying a regeneration

A consumer's `cargo check` only covers the `ndk` features that consumer enables (e.g. `data_space` sits behind
`#![cfg(feature = "api-level-28")]`, which Slint doesn't enable). Always also run:
`cargo check -p ndk --all-features --target aarch64-linux-android` in `external/ndk`.
Also diff the item names (`pub fn|struct|const|type|static`) old vs new and explain every removal.

## r30 vs upstream: removed items, explained

| Removed | Why | Impact |
|---|---|---|
| `INT_MAX`, `CHAR_BIT`, `SSIZE_MAX`, `UID_MAX`, … (libc limits) | clang 22 `limits.h` → `__INT_MAX__` builtins, which bindgen's macro evaluator can't resolve | none; use `core`/`libc` |
| `strto*_l`, `strerror_l`, `strptime_l` | now `__asm__("strtol")` aliases, which bindgen skips | none |
| `isinf`, `isnan` | now macros → `__builtin_isinf/isnan` | none |
| `__ANDROID_API__` | now `#define __ANDROID_API__ __ANDROID_MIN_SDK_VERSION__` | none |
| `STANDARD_BT2020_CONSTANT_LUMINANCE` | old header typo fixed (`ADATASPACE_` prefix) | `ndk` patched |

Notable additions relevant to uplink: `AImage_getTransform`, `AImageReader_setDefaultBufferSize`,
`AImageReader_setDefaultAHardwareBufferFormat`, `ACameraManager_openSharedCamera` + shared sessions,
`ANativeWindow_setProducerThrottlingEnabled`, AAudio device APIs, `APerformanceHint_*`.

## Workspace wiring

- Root `Cargo.toml`: `exclude = ["vendor", "sys"]`, so they're built as path deps but are not workspace members
  (clippy doesn't lint them). `[patch.crates-io]` points `ndk`/`ndk-sys` at `external/ndk/`.
- `sys/ndk-gl-sys` keeps bindgen's naming-lint allows (`non_upper_case_globals` etc.); the C names can't be renamed.
- Env (NDK/SDK/JAVA/LIBCLANG/SKIA cache) comes from the `justfile` exports, so run cargo through `just`.
- `ndk/src/media/media_codec.rs`, `media_format.rs`: imports (`abort_on_panic`, `c_char`, `c_void`, `Pin`, `Result`)
  and the `async_notify_callback` field are gated with the same `api-level-28`/`29` cfg as the code that uses them.
  Upstream leaves them ungated, which warns with `media` + `api-level-26`. Worth sending upstream.
