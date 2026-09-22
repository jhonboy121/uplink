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
    #[error("GL object creation returned 0")]
    GlObject,
    #[error(transparent)]
    Jni(#[from] jni::errors::Error),
    #[error(transparent)]
    Media(#[from] ndk::media_error::MediaError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("log filter: {0}")]
    LogFilter(#[from] tracing_subscriber::filter::ParseError),
    #[error(transparent)]
    LogInit(#[from] tracing_subscriber::util::TryInitError),
}
