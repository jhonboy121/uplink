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
android_platform := "android-37.0"
activity := "dev.uplink.UplinkActivity"
adb_user := "0"
llvm_cov := env("LLVM_COV", "/usr/bin/llvm-cov")
llvm_profdata := env("LLVM_PROFDATA", "/usr/bin/llvm-profdata")
coverage_dir := "target/coverage"
# proot emulates setsockopt via ptrace and returns EOPNOTSUPP under concurrent UDP binds (iroh/noq
# IP_PKTINFO); run tests serially here. Raise on real Linux/CI.
test_threads := env("RUST_TEST_THREADS", "1")
java_release := "8"

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
build_tools := ANDROID_HOME / "build-tools" / build_tools_version
apksigner := build_tools / "lib/apksigner.jar"
d8 := build_tools / "lib/d8.jar"
android_jar := ANDROID_HOME / "platforms" / android_platform / "android.jar"
d8_mode := if profile == "release" { "--release" } else { "--debug" }
java_src := "crates/uplink-android/java"
dex_dir := "target" / "dex" / profile
android_crates := "-p uplink -p uplink-android -p uplink-core"
host_crates := "-p axml -p ndk-bindgen -p uplink-core -p uplink-cli"

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

[doc("Type-check uplink-core for Android")]
check-core:
    nice cargo check -p uplink-core --target {{android_target}} --profile {{cargo_profile}}

[doc("Clippy (warnings are errors) for Android crates and host tools")]
clippy:
    nice cargo clippy {{android_crates}} --target {{android_target}} --profile {{cargo_profile}} -- -D warnings
    nice cargo clippy {{host_crates}} --all-targets -- -D warnings

[doc("Format the workspace")]
fmt:
    cargo fmt --all

[doc("Build the app library")]
build:
    nice cargo build -p {{lib}} --target {{android_target}} --profile {{cargo_profile}}

[doc("Compile the Java shim into dex_dir/classes.dex")]
dex:
    #!/bin/sh
    set -eu
    classes=$(mktemp -d)
    mkdir -p "{{dex_dir}}"
    trap 'rm -rf "$classes"' EXIT
    javac -source {{java_release}} -target {{java_release}} -bootclasspath "{{android_jar}}" -Xlint:-options \
        -d "$classes" $(find "{{java_src}}" -name '*.java')
    java -cp "{{d8}}" com.android.tools.r8.D8 {{d8_mode}} --min-api {{min_sdk}} --lib "{{android_jar}}" \
        --output "{{dex_dir}}" $(find "$classes" -name '*.class')

[doc('''
Build, strip, compile the Java shim, write the manifest, zip and sign the APK into out_dir.
Install from Termux with the printed termux-open command.
''')]
apk: build dex
    #!/bin/sh
    set -eu
    stage=$(mktemp -d)
    trap 'rm -rf "$stage"' EXIT
    mkdir -p "$stage/lib/arm64-v8a" "{{out_dir}}"
    strip --strip-debug -o "$stage/lib/arm64-v8a/lib{{lib}}.so" "{{so}}"
    cp "{{dex_dir}}/classes.dex" "$stage/"
    cargo run -q -p axml -- out="$stage/AndroidManifest.xml" package={{app_id}} label={{app_label}} \
        lib={{lib}} activity={{activity}} dex=1 min={{min_sdk}} target={{target_sdk}} version={{version_code}} \
        debuggable={{debuggable}} perm=android.permission.CAMERA
    (cd "$stage" && jar --create --no-manifest --file unsigned.apk AndroidManifest.xml classes.dex lib)
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

[doc("Install the APK and launch it over adb (needs a connected device)")]
run: apk
    adb install --user {{adb_user}} -r "{{apk}}"
    adb shell am start --user {{adb_user}} -n "{{app_id}}/{{activity}}"

[doc("Follow uplink's logcat output over adb (Java + native share the tag)")]
logcat:
    adb logcat -v time -s uplink:V AndroidRuntime:E DEBUG:F

[doc('''
Run the host CLI (uplink-core over iroh), e.g. `just cli --dir /tmp/a listen`.
Two instances with different --dir can call each other.
''')]
cli *args:
    nice cargo run -q -p uplink-cli -- {{args}}

[doc("Unit + integration tests for uplink-core (host, loopback only)")]
test *args:
    RUST_TEST_THREADS={{test_threads}} nice cargo test -p uplink-core {{args}}

[doc('''
Code coverage for uplink-core with cargo-llvm-cov, using the system LLVM tools (same major as rustc's).
HTML report in coverage_dir; `cargo llvm-cov clean --workspace` frees its instrumented build.
''')]
coverage:
    RUST_TEST_THREADS={{test_threads}} LLVM_COV="{{llvm_cov}}" LLVM_PROFDATA="{{llvm_profdata}}" nice cargo llvm-cov -p uplink-core --html --output-dir "{{coverage_dir}}"
    LLVM_COV="{{llvm_cov}}" LLVM_PROFDATA="{{llvm_profdata}}" cargo llvm-cov report -p uplink-core --summary-only
