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
    /// What Java actually said. `jni` reports any throw as the single word "JavaException" and
    /// leaves the throwable pending, so without lifting its own text across, the one sentence
    /// describing the failure never reaches the log.
    #[error("java threw: {0}")]
    Thrown(String),
    // No `#[from]`: both of these print as a bare variant name and nothing else — `MediaError`'s
    // Display is `{:?}` of itself — so an anonymous `?` puts "ErrorUnknown" in the log and the
    // reader is left to guess which of a dozen platform calls produced it. Naming the call is
    // the only thing that carries information, so the conversion has to be written out.
    #[error("{call}: {source:?}")]
    Media { call: &'static str, source: ndk::media_error::MediaError },
    #[error("{call}: {source:?}")]
    Audio { call: &'static str, source: ndk::audio::AudioError },
    #[error("voice stream disconnected while audio was being re-routed")]
    AudioRouting,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("log filter: {0}")]
    LogFilter(#[from] tracing_subscriber::filter::ParseError),
    #[error("a value does not fit the Java type it crosses as")]
    TooBig(#[from] std::num::TryFromIntError),
    /// Asked for something only a visible app can do — the window, the task, a permission prompt
    /// — while there is no activity. Not a failure of the call so much as of its timing.
    #[error("no activity: the app is not on screen")]
    NoActivity,
    #[error(transparent)]
    Core(#[from] uplink_core::Error),
}

/// Names the platform call a failure came from, for the errors that cannot describe themselves.
pub trait At<T> {
    fn at(self, call: &'static str) -> Result<T, Error>;
}

impl<T> At<T> for Result<T, ndk::media_error::MediaError> {
    fn at(self, call: &'static str) -> Result<T, Error> {
        self.map_err(|source| Error::Media { call, source })
    }
}

impl<T> At<T> for Result<T, ndk::audio::AudioError> {
    fn at(self, call: &'static str) -> Result<T, Error> {
        self.map_err(|source| Error::Audio { call, source })
    }
}
