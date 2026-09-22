//! H.264 through the platform's hardware MediaCodec in async mode, surface in and surface out.
//!
//! The NDK reports free input buffers, finished outputs and errors on its own thread; each codec
//! forwards those as [`Event`]s into a channel, so the codec can be driven from an async task.
//! The encoder hands out an input surface for the camera; the decoder renders into a
//! caller-provided surface (an `ImageReader` for the zero-copy preview).

use std::ffi::{CStr, c_char};

use ndk::media::media_codec::{AsyncNotifyCallback, BufferInfo, MediaCodec, MediaCodecDirection};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;
use ndk_sys as ffi;
use tokio::sync::mpsc;

use crate::Error;

/// `KEY_PRIORITY`: 0 is realtime, 1 best effort.
const REALTIME_PRIORITY: i32 = 0;
const ENABLED: i32 = 1;
/// `KEY_REQUEST_SYNC_FRAME` only needs to be present; the platform documents 0.
const REQUEST: i32 = 0;
const NO_FLAGS: u32 = 0;
const KEYFRAME_FLAG: u32 = ffi::AMEDIACODEC_BUFFER_FLAG_KEY_FRAME;
const CONFIG_FLAG: u32 = ffi::AMEDIACODEC_BUFFER_FLAG_CODEC_CONFIG;

/// Codec constants the NDK headers lack; read from the Java SDK by [`crate::platform::Platform`].
#[derive(Clone, Debug)]
pub struct Avc {
    pub mime: String,
    /// `MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface`.
    pub surface_color_format: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct VideoConfig {
    pub width: i32,
    pub height: i32,
    pub fps: i32,
    pub bitrate: i32,
    pub keyframe_interval_secs: i32,
}

/// An encoded access unit.
#[derive(Debug)]
pub struct Packet {
    pub data: Vec<u8>,
    pub presentation_micros: i64,
    pub keyframe: bool,
}

/// What the codec reports from its callback thread.
#[derive(Debug)]
pub enum Event {
    InputAvailable(usize),
    OutputAvailable(usize, BufferInfo),
    FormatChanged(String),
    Error { detail: String, fatal: bool },
}

pub type Events = mpsc::UnboundedReceiver<Event>;

/// # Safety
/// `name` must be one of the NDK's `AMEDIA*_KEY_*` string constants.
unsafe fn key(name: *const c_char) -> Result<&'static str, Error> {
    if name.is_null() {
        return Err(Error::MissingKey);
    }
    // SAFETY: the NDK key constants are static NUL-terminated ASCII strings.
    unsafe { CStr::from_ptr(name) }.to_str().map_err(|_| Error::MissingKey)
}

/// Reads an NDK key constant (`extern` statics in ndk-sys).
macro_rules! ndk_key {
    ($name:ident) => {
        // SAFETY: reads the pointer value of an NDK key constant, which is never written.
        unsafe { key(ffi::$name) }
    };
}

fn base_format(avc: &Avc, width: i32, height: i32) -> Result<MediaFormat, Error> {
    let mut format = MediaFormat::new();
    format.set_str(ndk_key!(AMEDIAFORMAT_KEY_MIME)?, &avc.mime);
    format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_WIDTH)?, width);
    format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_HEIGHT)?, height);
    format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_PRIORITY)?, REALTIME_PRIORITY);
    format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_LOW_LATENCY)?, ENABLED);
    Ok(format)
}

/// A `MediaCodec` in async mode, movable into the task that drives it.
struct Codec(MediaCodec);

// SAFETY: `AMediaCodec` serialises access internally. In async mode the platform itself calls
// back on the codec's looper thread while the client queues and releases buffers from another
// thread. We own the codec from one task at a time; the callbacks only send into a channel.
unsafe impl Send for Codec {}

impl Codec {
    /// Creates the codec with its callbacks registered (they must be set before `configure`).
    fn new(created: Option<MediaCodec>) -> Result<(Self, Events), Error> {
        let mut codec = created.ok_or(Error::NoCodec)?;
        let (tx, events) = mpsc::unbounded_channel();
        // A send fails only once the driving task is gone; the codec is being torn down then.
        let (input, output, format, error) = (tx.clone(), tx.clone(), tx.clone(), tx);
        codec.set_async_notify_callback(Some(AsyncNotifyCallback {
            on_input_available: Some(Box::new(move |index| drop(input.send(Event::InputAvailable(index))))),
            on_output_available: Some(Box::new(move |index, info| {
                drop(output.send(Event::OutputAvailable(index, *info)));
            })),
            on_format_changed: Some(Box::new(move |f| drop(format.send(Event::FormatChanged(f.to_string()))))),
            on_error: Some(Box::new(move |e, action, detail| {
                let fatal = !action.is_recoverable() && !action.is_transient();
                let detail = format!("{e}: {}", detail.to_string_lossy());
                drop(error.send(Event::Error { detail, fatal }));
            })),
        }))?;
        Ok((Self(codec), events))
    }

    fn name(&self) -> Option<String> {
        self.0.name().ok()
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        if let Err(e) = self.0.stop() {
            tracing::warn!("codec stop: {e}");
        }
    }
}

pub struct Encoder {
    codec: Codec,
    window: NativeWindow,
    /// SPS/PPS, prepended to every keyframe so any keyframe can start decoding.
    config: Vec<u8>,
}

impl Encoder {
    pub fn new(avc: &Avc, video: VideoConfig) -> Result<(Self, Events), Error> {
        let (codec, events) = Codec::new(MediaCodec::from_encoder_type(&avc.mime))?;
        let mut format = base_format(avc, video.width, video.height)?;
        format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_COLOR_FORMAT)?, avc.surface_color_format);
        format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_BIT_RATE)?, video.bitrate);
        format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_FRAME_RATE)?, video.fps);
        format.set_i32(ndk_key!(AMEDIAFORMAT_KEY_I_FRAME_INTERVAL)?, video.keyframe_interval_secs);
        codec.0.configure(&format, None, MediaCodecDirection::Encoder)?;
        let window = codec.0.create_input_surface()?;
        codec.0.start()?;
        tracing::info!(name = codec.name(), ?video, "encoder started");
        Ok((Self { codec, window, config: Vec::new() }, events))
    }

    /// The surface the camera renders into.
    pub const fn window(&self) -> &NativeWindow {
        &self.window
    }

    pub fn request_keyframe(&self) -> Result<(), Error> {
        let mut params = MediaFormat::new();
        params.set_i32(ndk_key!(AMEDIACODEC_KEY_REQUEST_SYNC_FRAME)?, REQUEST);
        Ok(self.codec.0.set_parameters(params)?)
    }

    /// Takes the output reported by [`Event::OutputAvailable`]; `None` for codec config, which is
    /// kept for the next keyframe.
    pub fn take_output(&mut self, index: usize, info: &BufferInfo) -> Result<Option<Packet>, Error> {
        let buffer = self.codec.0.output_buffer(index).ok_or(Error::NoCodecBuffer(index))?;
        let data = payload(buffer, info.offset(), info.size());
        let keyframe = info.flags() & KEYFRAME_FLAG != 0;
        let packet = if info.flags() & CONFIG_FLAG != 0 {
            self.config = data.to_vec();
            None
        } else if keyframe {
            Some([self.config.as_slice(), data].concat())
        } else {
            Some(data.to_vec())
        };
        self.codec.0.release_output_buffer_by_index(index, false)?;
        Ok(packet.map(|data| Packet { data, presentation_micros: info.presentation_time_us(), keyframe }))
    }
}

fn payload(buffer: &[u8], offset: i32, size: i32) -> &[u8] {
    let start = usize::try_from(offset).unwrap_or_default();
    let end = start.saturating_add(usize::try_from(size).unwrap_or_default());
    buffer.get(start..end).unwrap_or_default()
}

pub struct Decoder {
    codec: Codec,
}

impl Decoder {
    /// Decodes into `window`; the stream's own size wins over `width`x`height`.
    pub fn new(avc: &Avc, width: i32, height: i32, window: &NativeWindow) -> Result<(Self, Events), Error> {
        let (codec, events) = Codec::new(MediaCodec::from_decoder_type(&avc.mime))?;
        codec.0.configure(&base_format(avc, width, height)?, Some(window), MediaCodecDirection::Decoder)?;
        codec.0.start()?;
        tracing::info!(name = codec.name(), "decoder started");
        Ok((Self { codec }, events))
    }

    /// Fills the input buffer reported by [`Event::InputAvailable`] with one access unit.
    pub fn queue(&self, index: usize, data: &[u8], presentation_micros: u64, keyframe: bool) -> Result<(), Error> {
        let buffer = self.codec.0.input_buffer(index).ok_or(Error::NoCodecBuffer(index))?;
        let target = buffer.get_mut(..data.len()).ok_or(Error::FrameTooLarge(data.len()))?;
        target.write_copy_of_slice(data);
        let flags = if keyframe { KEYFRAME_FLAG } else { NO_FLAGS };
        Ok(self.codec.0.queue_input_buffer_by_index(index, 0, data.len(), presentation_micros, flags)?)
    }

    /// Renders the output reported by [`Event::OutputAvailable`] to the surface.
    pub fn render(&self, index: usize) -> Result<(), Error> {
        Ok(self.codec.0.release_output_buffer_by_index(index, true)?)
    }
}
