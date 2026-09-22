//! Builds the vendored libopus statically with its CMake project.

use std::path::Path;

fn main() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/opus");
    println!("cargo:rerun-if-changed={}", source.join("CMakeLists.txt").display());

    let mut config = cmake::Config::new(&source);
    config
        .profile("Release")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF");
    // CMake's Android mode wants the NDK's prebuilt (x86_64) clang; our CC wrapper already
    // targets Android with the NDK sysroot, so present it as a plain Linux cross compiler. The
    // wrapper links with -nodefaultlibs (for rustc), so CMake's probes must not link executables.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("android") {
        let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        config
            .define("CMAKE_SYSTEM_NAME", "Linux")
            .define("CMAKE_SYSTEM_PROCESSOR", arch)
            .define("CMAKE_TRY_COMPILE_TARGET_TYPE", "STATIC_LIBRARY");
    }
    let out = config.build();
    for dir in ["lib", "lib64"] {
        println!("cargo:rustc-link-search=native={}", out.join(dir).display());
    }
    println!("cargo:rustc-link-lib=static=opus");
}
