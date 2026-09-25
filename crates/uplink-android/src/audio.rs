//! AAudio voice capture and playback: 48 kHz mono `i16`, voice-communication mode so the
//! platform's echo canceller, noise suppressor and gain control apply.
//!
//! The data callbacks run on a realtime thread, so they only move samples through lock-free
//! rings; encoding and decoding happen on the caller's clock.
//!
//! Streams get disconnected whenever audio is re-routed (entering call mode, headphones,
//! Bluetooth). AAudio forbids closing or reopening a stream from its error callback, so the
//! callback only records it in [`AudioHealth`] and the caller reopens from its own thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ndk::audio::{
    AudioCallbackResult, AudioContentType, AudioDirection, AudioError, AudioFormat, AudioInputPreset,
    AudioPerformanceMode, AudioStream, AudioStreamBuilder, AudioStreamState, AudioUsage,
};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::Error;
use crate::error::At;

pub const SAMPLE_RATE: i32 = 48_000;
const CHANNELS: i32 = 1;
/// Ring capacity; well past the 20 ms the caller moves per tick, to absorb scheduling jitter.
const RING_SAMPLES: usize = 4800;

/// How the streams are faring; read by the caller to decide when to reopen.
#[derive(Debug, Default)]
pub struct AudioHealth {
    /// Samples the callbacks have moved, and how many were lost because the other side of a ring
    /// wasn't serviced in time.
    pub captured_samples: AtomicU64,
    pub played_samples: AtomicU64,
    pub captured: AtomicU64,
    pub played: AtomicU64,
    /// A stream reported an error, so both must be closed and opened again.
    disconnected: AtomicBool,
}

impl AudioHealth {
    pub fn disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed)
    }
}

/// The open streams; dropping them closes both.
pub struct Streams {
    capture: AudioStream,
    playback: AudioStream,
}

impl Streams {
    /// True once either stream is gone. A stream disconnected before it ever ran gets no error
    /// callback, so its state has to be polled as well.
    pub fn disconnected(&self) -> bool {
        [&self.capture, &self.playback].into_iter().any(|stream| {
            matches!(
                stream.state(),
                AudioStreamState::Disconnected | AudioStreamState::Closing | AudioStreamState::Closed
            )
        })
    }
}

/// The caller's ends of the rings, renewed every time the streams are opened.
pub struct Rings {
    pub microphone: Consumer<i16>,
    pub speaker: Producer<i16>,
}

fn builder(
    direction: AudioDirection,
    health: &Arc<AudioHealth>,
    what: &'static str,
) -> Result<AudioStreamBuilder, Error> {
    let health = Arc::clone(health);
    Ok(AudioStreamBuilder::new()
        .at("AAudio_createStreamBuilder")?
        .direction(direction)
        .format(AudioFormat::PCM_I16)
        .channel_count(CHANNELS)
        .sample_rate(SAMPLE_RATE)
        .performance_mode(AudioPerformanceMode::LowLatency)
        .usage(AudioUsage::VoiceCommunication)
        .content_type(AudioContentType::Speech)
        // Never close or reopen here: AAudio calls this on the stream's own thread.
        .error_callback(Box::new(move |_, e: AudioError| {
            tracing::warn!("{what} stream: {e}");
            health.disconnected.store(true, Ordering::Relaxed);
        })))
}

/// Samples of one callback buffer.
///
/// # Safety
/// `data` must point to `frames` `i16`s, as AAudio guarantees for a PCM_I16 mono stream.
unsafe fn samples<'a>(data: *mut std::ffi::c_void, frames: i32) -> &'a mut [i16] {
    let len = usize::try_from(frames).unwrap_or_default();
    // SAFETY: per the caller contract.
    unsafe { std::slice::from_raw_parts_mut(data.cast::<i16>(), len) }
}

/// Opens and starts both streams, with fresh rings for the caller.
pub fn open(health: &Arc<AudioHealth>) -> Result<(Streams, Rings), Error> {
    let (mut into_ring, microphone) = RingBuffer::new(RING_SAMPLES);
    let (speaker, mut from_ring) = RingBuffer::new(RING_SAMPLES);
    let captured = Arc::clone(health);
    let capture = builder(AudioDirection::Input, health, "capture")?
        .input_preset(AudioInputPreset::VoiceCommunication)
        .data_callback(Box::new(move |_, data, frames| {
            // SAFETY: AAudio passes `frames` mono i16 samples for this stream's format.
            let samples = unsafe { samples(data, frames) };
            captured
                .captured_samples
                .fetch_add(u64::try_from(samples.len()).unwrap_or_default(), Ordering::Relaxed);
            let lost = samples.iter().filter(|&&sample| into_ring.push(sample).is_err()).count();
            if lost > 0 {
                captured.captured.fetch_add(u64::try_from(lost).unwrap_or_default(), Ordering::Relaxed);
            }
            AudioCallbackResult::Continue
        }))
        .open_stream()
        .at("AAudioStreamBuilder_openStream (capture)")?;
    capture.request_start().at("AAudioStream_requestStart (capture)")?;

    let played = Arc::clone(health);
    let playback = builder(AudioDirection::Output, health, "playback")?
        .data_callback(Box::new(move |_, data, frames| {
            // SAFETY: AAudio expects `frames` mono i16 samples for this stream's format.
            let samples = unsafe { samples(data, frames) };
            played.played_samples.fetch_add(u64::try_from(samples.len()).unwrap_or_default(), Ordering::Relaxed);
            let mut missing = 0;
            for sample in samples.iter_mut() {
                *sample = from_ring.pop().unwrap_or_else(|_| {
                    missing += 1;
                    0
                });
            }
            if missing > 0 {
                played.played.fetch_add(missing, Ordering::Relaxed);
            }
            AudioCallbackResult::Continue
        }))
        .open_stream()
        .at("AAudioStreamBuilder_openStream (playback)")?;
    playback.request_start().at("AAudioStream_requestStart (playback)")?;

    let streams = Streams { capture, playback };
    let (capture, playback) = (&streams.capture, &streams.playback);
    // Entering call mode re-routes audio and can disconnect a stream as it opens; the caller
    // retries, by which time routing has settled.
    if streams.disconnected() {
        return Err(Error::AudioRouting);
    }
    health.disconnected.store(false, Ordering::Relaxed);
    tracing::info!(
        rate = capture.sample_rate(),
        burst = capture.frames_per_burst(),
        capture_state = ?capture.state(),
        capture_preset = ?capture.input_preset(),
        capture_channels = capture.channel_count(),
        capture_format = ?capture.format(),
        playback_state = ?playback.state(),
        playback_rate = playback.sample_rate(),
        "voice streams open"
    );
    Ok((streams, Rings { microphone, speaker }))
}
