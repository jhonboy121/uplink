set shell := ["sh", "-eu", "-c"]

# ---- overridable: `just profile=release log=debug apk` ----
profile := "debug"
log := "info"
android_target := "aarch64-linux-android"
min_sdk := "30"
target_sdk := "37"
app_id := "dev.uplink"
app_label := "Uplink"
version_code := "1"
out_dir := home_directory() / "android/out"
keystore := home_directory() / "android/debug.keystore"
keystore_pass := "android"
keystore_validity_days := "10000"
build_tools_version := "37.0.0"

export ANDROID_NDK_HOME := env("ANDROID_NDK_HOME", home_directory() / "android/ndk")
export ANDROID_NDK := ANDROID_NDK_HOME
export ANDROID_HOME := env("ANDROID_HOME", home_directory() / "android/sdk")
export ANDROID_SDK_ROOT := ANDROID_HOME
export ANDROID_API := min_sdk
export JAVA_HOME := env("JAVA_HOME", "/usr/lib/jvm/java-17-openjdk")
export LIBCLANG_PATH := env("LIBCLANG_PATH", "/usr/lib/llvm22/lib")
export SKIA_BINARIES_URL := "file://" + home_directory() / "android/cache/skia-binaries-{key}.tar.gz"
export UPLINK_LOG := log

cargo_profile := if profile == "release" { "release" } else { "dev" }
debuggable := if profile == "release" { "0" } else { "1" }
lib := "uplink"
so := "target" / android_target / profile / ("lib" + lib + ".so")
apk := out_dir / "uplink.apk"
apksigner := ANDROID_HOME / "build-tools" / build_tools_version / "lib/apksigner.jar"
android_crates := "-p uplink -p uplink-android"
host_crates := "-p axml -p ndk-bindgen"

[doc("List recipes")]
default:
    @just --list

[doc('''
Regenerate external/ndk/ndk-sys + sys/ndk-gl-sys bindings from the NDK sysroot.
Review the diff afterwards (docs/ref/ndk-vendoring.md).
''')]
bindgen:
    nice cargo run -q -p ndk-bindgen

[doc("Type-check the Android crates")]
check:
    nice cargo check {{android_crates}} --target {{android_target}} --profile {{cargo_profile}}

[doc("Clippy (warnings are errors) for Android crates and host tools")]
clippy:
    nice cargo clippy {{android_crates}} --target {{android_target}} --profile {{cargo_profile}} -- -D warnings
    nice cargo clippy {{host_crates}} -- -D warnings

[doc("Format the workspace")]
fmt:
    cargo fmt --all

[doc("Build the app library")]
build:
    nice cargo build -p {{lib}} --target {{android_target}} --profile {{cargo_profile}}

[doc('''
Build, strip, write the manifest, zip and sign the APK into out_dir.
Install from Termux with the printed termux-open command.
''')]
apk: build
    #!/bin/sh
    set -eu
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT
    mkdir -p "$stage/lib/arm64-v8a" "{{out_dir}}"
    strip --strip-debug -o "$stage/lib/arm64-v8a/lib{{lib}}.so" "{{so}}"
    cargo run -q -p axml -- out="$stage/AndroidManifest.xml" package={{app_id}} label={{app_label}} \
        lib={{lib}} min={{min_sdk}} target={{target_sdk}} version={{version_code}} \
        debuggable={{debuggable}} perm=android.permission.CAMERA
    (cd "$stage" && jar --create --no-manifest --file unsigned.apk AndroidManifest.xml lib)
    if [ ! -f "{{keystore}}" ]; then
        keytool -genkeypair -keystore "{{keystore}}" -storepass "{{keystore_pass}}" -keypass "{{keystore_pass}}" \
            -alias debug -keyalg RSA -validity {{keystore_validity_days}} -dname "CN=uplink debug" >/dev/null
    fi
    java -jar "{{apksigner}}" sign --v4-signing-enabled false --ks "{{keystore}}" --ks-pass "pass:{{keystore_pass}}" \
        --out "{{apk}}" "$stage/unsigned.apk"
    echo "{{apk}} ($(du -h "{{apk}}" | cut -f1))"
    echo "install (Termux): termux-open {{replace(apk, home_directory(), '~/alpine-data')}}"

[doc("Disk usage of target/ per profile")]
size:
    du -sh target/*/ target/*/*/ 2>/dev/null | sort -h

[doc("Delete build outputs")]
clean:
    cargo clean
