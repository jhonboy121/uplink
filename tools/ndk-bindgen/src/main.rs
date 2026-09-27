//! Regenerates `ffi_<arch>.rs` for `external/ndk/ndk-sys` (upstream-compatible flags) and
//! `sys/ndk-gl-sys` (EGL/GLES3) from a local NDK sysroot.
//!
//! usage: ndk-bindgen [<sysroot>]  (default: $ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot)

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};

const TARGETS: [(&str, &str); 4] = [
    ("arm", "arm-linux-androideabi"),
    ("aarch64", "aarch64-linux-android"),
    ("i686", "i686-linux-android"),
    ("x86_64", "x86_64-linux-android"),
];

/// `__ANDROID_API_FUTURE__`: all APIs are plain declarations, as with upstream's unversioned
/// triple (r30 rejects unversioned triples).
const ANDROID_API_FUTURE: u32 = 10000;

const PREBUILT_SYSROOT: &str = "toolchains/llvm/prebuilt/linux-x86_64/sysroot";

/// Upstream ndk-sys targets Rust 1.60 / edition 2021.
const NDK_SYS_RUST_MINOR: u64 = 60;
/// Edition 2024 needs 1.85 for `unsafe extern` blocks.
const NDK_GL_SYS_RUST_MINOR: u64 = 85;

const NDK_SYS_BLOCKLIST: [&str; 4] = [r"JNI\w+", r"C?_?JNIEnv", r"_?JavaVM", r"_?j\w+"];

/// libc prototypes whose `c_ulong` size_t trips rustc's runtime-symbol lint; unused by `ndk`.
const NDK_SYS_BLOCKLIST_FUNCTIONS: [&str; 5] = ["memcmp", "memcpy", "memmove", "memset", "strlen"];

const NDK_SYS_NEWTYPE_ENUMS: [&str; 40] = [
    r"\w+_(result|status)_t",
    "ACameraDevice_request_template",
    "ADataSpace",
    "AHardwareBuffer_Format",
    "AHardwareBuffer_UsageFlags",
    "AHdrMetadataType",
    "AIMAGE_FORMATS",
    "AMediaDrmEventType",
    "AMediaDrmKeyRequestType",
    "AMediaDrmKeyType",
    "AMediaKeyStatusType",
    "AMidiDevice_Protocol",
    "AMotionClassification",
    "ANativeWindowTransform",
    "ANativeWindow_ChangeFrameRateStrategy",
    "ANativeWindow_FrameRateCompatibility",
    "ANativeWindow_LegacyFormat",
    "AndroidBitmapCompressFormat",
    "AndroidBitmapFormat",
    "AppendMode",
    "DeviceTypeCode",
    "DurationCode",
    "FeatureLevelCode",
    "FuseCode",
    "HeapTaggingLevel",
    "OperandCode",
    "OperationCode",
    "OutputFormat",
    "PaddingCode",
    "PreferenceCode",
    "PriorityCode",
    "ResNsendFlags",
    "ResultCode",
    "SeekMode",
    r"acamera_\w+",
    "android_LogPriority",
    "android_fdsan_error_level",
    "android_fdsan_owner_type",
    "cryptoinfo_mode_t",
    "log_id",
];

type Configure = fn(bindgen::Builder) -> Result<bindgen::Builder>;

fn rust_target(minor: u64) -> Result<bindgen::RustTarget> {
    bindgen::RustTarget::stable(minor, 0).map_err(|e| anyhow!("rust target 1.{minor}: {e}"))
}

fn ndk_sys(b: bindgen::Builder) -> Result<bindgen::Builder> {
    let b = NDK_SYS_BLOCKLIST.iter().fold(b, |b, item| b.blocklist_item(*item));
    let b = NDK_SYS_BLOCKLIST_FUNCTIONS.iter().fold(b, |b, f| b.blocklist_function(*f));
    let b = NDK_SYS_NEWTYPE_ENUMS.iter().fold(b, |b, e| b.newtype_enum(*e));
    Ok(b.rust_target(rust_target(NDK_SYS_RUST_MINOR)?))
}

fn ndk_gl_sys(b: bindgen::Builder) -> Result<bindgen::Builder> {
    Ok(b.rust_target(rust_target(NDK_GL_SYS_RUST_MINOR)?)
        .rust_edition(bindgen::RustEdition::Edition2024)
        .allowlist_function("(egl|gl)[A-Z].*")
        .allowlist_var("(EGL|GL)_.*")
        .allowlist_type("(EGL|GL|Khronos|khronos_).*"))
}

const JOBS: [(&str, Configure); 2] = [("external/ndk/ndk-sys", ndk_sys), ("sys/ndk-gl-sys", ndk_gl_sys)];

fn default_sysroot() -> Result<PathBuf> {
    let ndk = std::env::var_os("ANDROID_NDK_HOME").context("ANDROID_NDK_HOME not set (run via `just bindgen`)")?;
    Ok(PathBuf::from(ndk).join(PREBUILT_SYSROOT))
}

fn main() -> Result<()> {
    let sysroot = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => default_sysroot()?,
    };
    ensure!(sysroot.join("usr/include").is_dir(), "no NDK sysroot at {}", sysroot.display());
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("tools/ndk-bindgen must live two levels below the workspace root")?;

    for (dir, configure) in JOBS {
        let crate_dir = workspace.join(dir);
        for (arch, target) in TARGETS {
            let builder = bindgen::Builder::default()
                .header(crate_dir.join("wrapper.h").to_string_lossy())
                .clang_arg(format!("--sysroot={}", sysroot.display()))
                .clang_arg(format!("--target={target}{ANDROID_API_FUTURE}"))
                // Weak mode keeps every declaration visible regardless of min SDK.
                .clang_arg("-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__");
            let out = crate_dir.join(format!("src/ffi_{arch}.rs"));
            configure(builder)?.generate().with_context(|| format!("{dir} ({target})"))?.write_to_file(&out)?;
            println!("wrote {}", out.display());
        }
    }
    Ok(())
}
