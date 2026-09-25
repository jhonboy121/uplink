set shell := ["sh", "-eu", "-c"]

# ---- overridable: `just profile=release log=debug apk` ----
profile := "debug"
# iroh's path events always: they say why a call ran over the path it did, and only fire on changes.
log := "info,iroh::_events::path=debug,iroh::socket::biased_rtt_path_selector=trace"
android_target := "aarch64-linux-android"
# Only what the manifest shares with the build; the rest of the app's identity (label, versions,
# targetSdk, its other components) is written in android/AndroidManifest.xml.
# Android 12: the NDK's MediaCodec keys (keyframe on request, bitrate) and CallStyle start there.
min_sdk := "31"
app_id := "dev.uplink"
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
# The design's frame in dp: 360 wide at the mockup's 9/19.3, which is what `just ui-diff` renders.
design_size := "360x772"
# The TUI's log, where `just cli` puts it.
cli_log := env("UPLINK_CLI_LOG", "target/cli.log")
java_release := "8"
# google-java-format's all-deps jar; 1.28.0 is the last that runs on JDK 17.
google_java_format := env("GOOGLE_JAVA_FORMAT", home_directory() / ".local/share/java/google-java-format.jar")

export ANDROID_NDK_HOME := env("ANDROID_NDK_HOME", home_directory() / "android/ndk")
export ANDROID_NDK := ANDROID_NDK_HOME
export ANDROID_HOME := env("ANDROID_HOME", home_directory() / "android/sdk")
export ANDROID_SDK_ROOT := ANDROID_HOME
export ANDROID_API := min_sdk
# Debian's openjdk-17 directory; Slint's build script runs javac + d8 from it.
export JAVA_HOME := env("JAVA_HOME", "/usr/lib/jvm/java-17-openjdk-" + if arch() == "aarch64" { "arm64" } else { "amd64" })
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

[doc('''
Format Rust (cargo fmt), TOML (taplo) and Java (google-java-format), each by its default rules.
vendor/ stays as upstream wrote it: plain `cargo fmt` formats the members, not path dependencies.
''')]
fmt:
    cargo fmt
    git ls-files '*.toml' ':!vendor/' | RUST_LOG=warn xargs taplo fmt
    git ls-files '*.java' ':!vendor/' | xargs java -jar "{{google_java_format}}" --replace

[doc("Fail if anything `just fmt` would change")]
fmt-check:
    cargo fmt --check
    git ls-files '*.toml' ':!vendor/' | RUST_LOG=warn xargs taplo fmt --check
    git ls-files '*.java' ':!vendor/' | xargs java -jar "{{google_java_format}}" --dry-run --set-exit-if-changed

[doc("Build the app library")]
build:
    nice cargo build -p {{lib}} --target {{android_target}} --profile {{cargo_profile}}

[doc("Write assets/icons' SVGs into android/res/drawable as <vector> drawables (checked in)")]
drawables:
    cargo run -q -p android-res -- vectors

[doc("Compile the Java shim into dex_dir/classes.dex")]
dex:
    #!/bin/sh
    set -eu
    classes=$(mktemp -d)
    generated=$(mktemp -d)
    mkdir -p "{{dex_dir}}"
    trap 'rm -rf "$classes" "$generated"' EXIT
    # R.java from the same res/ the APK's table is compiled from, so the ids agree.
    cargo run -q -p android-res -- r-class --package {{app_id}} --out "$generated"
    javac -source {{java_release}} -target {{java_release}} -bootclasspath "{{android_jar}}" -Xlint:-options \
        -d "$classes" $(find "{{java_src}}" "$generated" -name '*.java')
    java -cp "{{d8}}" com.android.tools.r8.D8 {{d8_mode}} --min-api {{min_sdk}} --lib "{{android_jar}}" \
        --output "{{dex_dir}}" $(find "$classes" -name '*.class')

[doc('''
Build, strip, compile the Java shim, write the manifest, zip and sign the APK into out_dir.
Install with `just install` (adb).
''')]
apk: build dex
    #!/bin/sh
    set -eu
    stage=$(mktemp -d)
    unsigned="$stage.apk"
    trap 'rm -rf "$stage" "$unsigned"' EXIT
    mkdir -p "$stage/lib/arm64-v8a" "{{out_dir}}"
    llvm-strip --strip-debug -o "$stage/lib/arm64-v8a/lib{{lib}}.so" "{{so}}"
    cp "{{dex_dir}}/classes.dex" "$stage/"
    cargo run -q -p android-res -- compile --out "$stage" \
        --define package={{app_id}} --define lib={{lib}} --define activity={{activity}} \
        --define minSdk={{min_sdk}} \
        --define debuggable={{ if debuggable == "1" { "true" } else { "false" } }}
    cargo run -q -p android-res -- package --dir "$stage" --out "$unsigned"
    if [ ! -f "{{keystore}}" ]; then
        keytool -genkeypair -keystore "{{keystore}}" -storepass "{{keystore_pass}}" -keypass "{{keystore_pass}}" \
            -alias debug -keyalg RSA -validity {{keystore_validity_days}} -dname "CN=uplink debug" >/dev/null
    fi
    java -jar "{{apksigner}}" sign --v4-signing-enabled false --ks "{{keystore}}" --ks-pass "pass:{{keystore_pass}}" \
        --out "{{apk}}" "$unsigned"
    echo "{{apk}} ($(du -h "{{apk}}" | cut -f1))"

[doc("Disk usage of target/ per profile")]
size:
    du -sh target/*/ target/*/*/ 2>/dev/null | sort -h

[doc("Delete build outputs")]
clean:
    cargo clean

[doc("Build and install the APK and launch it over adb (needs a connected device)")]
run: apk install

[doc("Install the APK and launch it over adb (needs a connected device)")]
install:
    adb install --user {{adb_user}} -r "{{apk}}"
    adb shell am start --user {{adb_user}} -n "{{app_id}}/{{activity}}"

[doc("Follow uplink's logcat output over adb (Java + native share the tag)")]
logcat:
    adb logcat -v time -s uplink:V AndroidRuntime:E DEBUG:F

[doc("Add COUNT random contacts to the app on the device over adb (debug build; restarts the app)")]
seed count="20":
    python3 tools/devdb/devdb.py --package {{app_id}} --user {{adb_user}} --launch {{activity}} seed {{count}}

[doc("Run one SQL STATEMENT against the app's database on the device, for a hand migration (debug build; restarts the app)")]
db-sql statement:
    python3 tools/devdb/devdb.py --package {{app_id}} --user {{adb_user}} --launch {{activity}} sql "{{statement}}"

[doc("Save KEY as a contact on the device, e.g. the one the CLI prints (debug build; restarts the app)")]
add-contact key name="CLI":
    python3 tools/devdb/devdb.py --package {{app_id}} --user {{adb_user}} --launch {{activity}} add {{key}} "{{name}}"

[doc('''
The call TUI: a phone in the terminal (uplink-core over iroh), e.g.
`just cli --video ~/uplink-media/clip.mp4 --record target/rec.mp4`. Two with different --dir can call
each other; `just cli --dir /tmp/a id` / `add <name> <key>` / `contacts` manage who it can call.
Built with the `opt` profile: it encodes live, which a debug build cannot keep up with. Its log is target/cli.log ($UPLINK_CLI_LOG).
''')]
cli *args:
    UPLINK_CLI_LOG={{cli_log}} nice cargo run -q --profile opt -p uplink-cli -- {{args}}

[doc('''
Render every screen of the UI to target/ui-preview with Slint's software renderer.
Checks layout without flashing an APK; fonts and camera frames are the host's stand-ins.
Each screen gets a .png and a .txt table of every element's position, size and paint;
the debug-info variable is what puts the markup's own element names in that table.
''')]
preview:
    SLINT_EMIT_DEBUG_INFO=1 nice cargo run -q -p ui-preview

[doc('''
Rewrite crates/uplink/lang/uplink.pot from every @tr in crates/uplink/ui — the template a new
language's .po starts from.
''')]
tr-pot:
    python3 tools/tr/tr.py pot

[doc('''
List what each language's .po lacks, leaves empty, or still has after the markup dropped it; the
same for android/res/values-<lang>/ against values/; and translations whose placeholders differ.
Slint keys a string by its text and the component it is in, so moving one is a new string.
''')]
tr-check:
    python3 tools/tr/tr.py check

[doc('''
Regenerate tools/android-res/src/framework.rs — every android: attribute and style id, and the named
values of configChanges, launchMode, foregroundServiceType and windowSoftInputMode, read out of
android.jar with javap. Run it when the compile SDK changes; the result is checked in.
''')]
android-table:
    javap -constants -cp "{{android_jar}}" 'android.R$attr' 'android.R$style' \
        android.content.pm.ActivityInfo android.content.pm.ServiceInfo 'android.view.WindowManager$LayoutParams' \
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

[doc('''
A bad network for this box's calls, both ways (sudo), to test rate control and reconnecting
against the phone: `just netem 3g`, `just netem cut 10` (undone after 10 s), or
`just netem custom "rate 2mbit loss 3%"`. Profiles: 3g, 4g-bad, slow, lossy, jitter, squeeze,
cut. Only UDP and TCP 443 are shaped; SSH and the adb tunnel never are. See tools/netem/netem.sh.
''')]
netem profile seconds="":
    sudo sh tools/netem/netem.sh {{profile}} {{quote(seconds)}}

[doc("Back to the plain network after `just netem`")]
netem-off:
    sudo sh tools/netem/netem.sh off

[doc("What `just netem` has applied now")]
netem-show:
    sh tools/netem/netem.sh show

[doc("Unit + integration tests for uplink-core (host, loopback only)")]
test *args:
    nice cargo test -p uplink-core {{args}}

[doc('''
Code coverage for uplink-core with cargo-llvm-cov, using the system LLVM tools (same major as rustc's).
HTML report in coverage_dir; `cargo llvm-cov clean --workspace` frees its instrumented build.
''')]
coverage:
    LLVM_COV="{{llvm_cov}}" LLVM_PROFDATA="{{llvm_profdata}}" nice cargo llvm-cov -p uplink-core --html --output-dir "{{coverage_dir}}"
    LLVM_COV="{{llvm_cov}}" LLVM_PROFDATA="{{llvm_profdata}}" cargo llvm-cov report -p uplink-core --summary-only
