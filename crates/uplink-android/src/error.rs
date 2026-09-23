use crate::camera::Facing;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{call} failed: camera_status_t {status}")]
    Camera { call: &'static str, status: i32 },
    #[error("no {0:?} camera")]
    NoCamera(Facing),
    #[error("{call} failed: EGL error {code:#x}")]
    Egl { call: &'static str, code: i32 },
    #[error("{call} failed: GL error {code:#x}")]
    Gl { call: &'static str, code: u32 },
    #[error("shader: {0}")]
    Shader(String),
    #[error("native handle does not fit a jlong on this target")]
    Handle,
    #[error("request abandoned before the activity answered")]
    RequestAbandoned,
    #[error("no H.264 codec on this device")]
    NoCodec,
    #[error("MediaCodec key constant unavailable")]
    MissingKey,
    #[error("codec returned no buffer for index {0}")]
    NoCodecBuffer(usize),
    #[error("frame of {0} bytes exceeds the codec buffer")]
    FrameTooLarge(usize),
    #[error("GL object creation returned 0")]
    GlObject,
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
    #[error(transparent)]
    Media(#[from] ndk::media_error::MediaError),
    #[error(transparent)]
    Audio(#[from] ndk::audio::AudioError),
    #[error("voice stream disconnected while audio was being re-routed")]
    AudioRouting,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("log filter: {0}")]
    LogFilter(#[from] tracing_subscriber::filter::ParseError),
    #[error("a value does not fit the Java type it crosses as")]
    TooBig(#[from] std::num::TryFromIntError),
    #[error(transparent)]
    Core(#[from] uplink_core::Error),
}
