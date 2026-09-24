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
call_service := "dev.uplink.UplinkCallService"
listen_service := "dev.uplink.UplinkListenService"
boot_receiver := "dev.uplink.UplinkBootReceiver"
files_provider := "dev.uplink.UplinkFiles"
application := "dev.uplink.UplinkApplication"
adb_user := "0"
llvm_cov := env("LLVM_COV", "/usr/bin/llvm-cov")
llvm_profdata := env("LLVM_PROFDATA", "/usr/bin/llvm-profdata")
coverage_dir := "target/coverage"
# The design's frame in dp: 360 wide at the mockup's 9/19.3, which is what `just ui-diff` renders.
design_size := "360x772"
cli_log := env("UPLINK_CLI_LOG", "target/cli.log")
# proot emulates setsockopt via ptrace and returns EOPNOTSUPP under concurrent UDP binds (iroh/noq
# IP_PKTINFO); run tests serially here. Raise on real Linux/CI.
test_threads := env("RUST_TEST_THREADS", "1")
java_release := "8"

# rustc mmaps its incremental cache; under proot that faults (SIGBUS) when linking the app.
# It also keeps the target dir smaller. Override with CARGO_INCREMENTAL=1 if it ever behaves.
export CARGO_INCREMENTAL := env("CARGO_INCREMENTAL", "0")

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
host_crates := "-p android-res -p ndk-bindgen -p uplink-core -p uplink-cli -p ui-preview"

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
    unsigned="$stage.apk"
    trap 'rm -rf "$stage" "$unsigned"' EXIT
    mkdir -p "$stage/lib/arm64-v8a" "{{out_dir}}"
    strip --strip-debug -o "$stage/lib/arm64-v8a/lib{{lib}}.so" "{{so}}"
    cp "{{dex_dir}}/classes.dex" "$stage/"
    cargo run -q -p android-res -- compile --out "$stage" \
        --define package={{app_id}} --define label={{app_label}} --define lib={{lib}} \
        --define activity={{activity}} --define service={{call_service}} \
        --define listen_service={{listen_service}} --define boot_receiver={{boot_receiver}} \
        --define provider={{files_provider}} --define application={{application}} \
        --define minSdk={{min_sdk}} --define targetSdk={{target_sdk}} \
        --define versionCode={{version_code}} --define versionName=0.{{version_code}} \
        --define debuggable={{ if debuggable == "1" { "true" } else { "false" } }}
    cargo run -q -p android-res -- package --dir "$stage" --out "$unsigned"
    if [ ! -f "{{keystore}}" ]; then
        keytool -genkeypair -keystore "{{keystore}}" -storepass "{{keystore_pass}}" -keypass "{{keystore_pass}}" \
            -alias debug -keyalg RSA -validity {{keystore_validity_days}} -dname "CN=uplink debug" >/dev/null
    fi
    java -jar "{{apksigner}}" sign --v4-signing-enabled false --ks "{{keystore}}" --ks-pass "pass:{{keystore_pass}}" \
        --out "{{apk}}" "$unsigned"
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

[doc('''
Run the host CLI with its output (reports + logs) also kept in target/cli.log ($UPLINK_CLI_LOG), e.g.
`just cli-log call phone --video ~/uplink-media/clip.mp4`.
''')]
cli-log *args:
    set -o pipefail; nice cargo run -q -p uplink-cli -- {{args}} 2>&1 | tee {{cli_log}}

[doc('''
Render every screen of the UI to target/ui-preview with Slint's software renderer.
Checks layout without flashing an APK; fonts and camera frames are the host's stand-ins.
Each screen gets a .png and a .txt table of every element's position, size and paint;
the debug-info variable is what puts the markup's own element names in that table.
''')]
preview:
    SLINT_EMIT_DEBUG_INFO=1 nice cargo run -q -p ui-preview

[doc('''
Regenerate tools/android-res/src/framework.rs — every android: attribute and style id, read out
of android.jar with javap. Run it when the compile SDK changes; the result is checked in.
''')]
android-table:
    javap -constants -cp "{{android_jar}}" 'android.R$attr' 'android.R$style' \
        | cargo run -q -p android-res -- gen-table --out tools/android-res/src/framework.rs

[doc('''
Measure the locked design (docs/design/uplink-call-ui.html) in headless Chromium.
The page reports its own geometry once the webfonts have loaded, to target/design/geometry.json —
what the CSS resolves to, which reading the stylesheet cannot tell you.
''')]
design-measure:
    nice python3 tools/design-probe/probe.py

[doc('''
Print one measured design screen as a table, scaled to a 360dp phone, in the same shape as the
element tables `just preview` writes — e.g. `just design-table People`. Run design-measure first.
''')]
design-table screen="People":
    python3 tools/design-probe/table.py {{screen}}

[doc('''
Screenshot one design screen to target/design/<screen>.png at 360dp, with the mockup's drawn
status bar removed — Android draws that, not the app — so it lines up with our own render.
''')]
design-shot screen="People":
    nice python3 tools/design-probe/probe.py {{screen}}

[doc('''
Render the build at the design's own frame and compare the two by bands of content: every row of
each image is background or ink, which is comparable even while the design is light and the build
dark. Prints each band's position and size, and the difference. e.g. `just ui-diff people People`.
''')]
ui-diff name="people" screen="People": (design-shot screen)
    UPLINK_PREVIEW_SIZE={{design_size}} nice cargo run -q -p ui-preview
    nice cargo run -q -p ui-preview -- target/design/{{lowercase(replace(screen, " ", "-"))}}.png target/ui-preview/{{name}}.png

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
