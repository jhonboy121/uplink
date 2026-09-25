//! Call audio: microphone → Opus → peer, and peer → Opus → speaker, moved between the AAudio
//! rings and the codecs on a 20 ms clock.
//!
//! Streams are reopened when AAudio disconnects them (route changes, headphones). Reopening
//! makes new rings, which are handed to the running pump, so the call and codecs carry on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uplink_android::audio::{AudioHealth, Rings, Streams};
use uplink_core::audio::{AudioReceiver, AudioSender, FRAME_DURATION, FRAME_SAMPLES, Pcm};

use crate::tasks::Tasks;

/// One spare slot is enough: only the newest rings matter.
const SWAP_QUEUE: usize = 1;

/// What the stream callbacks did over one stats interval.
pub struct VoiceCounts {
    pub captured: u64,
    pub played: u64,
    pub capture_lost: u64,
    pub playback_lost: u64,
}

/// Field order is drop order: the pump stops before the streams close.
pub struct CallAudio {
    tasks: Tasks,
    /// `None` while the streams are down, waiting for [`Self::reopen`].
    streams: Option<Streams>,
    swap: mpsc::Sender<Rings>,
    muted: Arc<AtomicBool>,
    pub health: Arc<AudioHealth>,
}

impl CallAudio {
    /// Starts the pump; the streams follow, and a failure here is recoverable by [`Self::reopen`].
    pub fn start(sender: AudioSender, receiver: AudioReceiver, runtime: Handle) -> Self {
        let health = Arc::<AudioHealth>::default();
        let (swap, rings) = mpsc::channel(SWAP_QUEUE);
        let muted = Arc::<AtomicBool>::default();
        let mut tasks = Tasks::new(runtime);
        let cancel = tasks.cancel_token();
        tasks.spawn(pump(rings, sender, receiver, Arc::clone(&muted), cancel));
        let mut audio = Self { tasks, streams: None, swap, muted, health };
        if let Err(e) = audio.reopen() {
            tracing::error!("voice streams: {e:#}");
        }
        audio
    }

    /// Whether the streams need reopening; call from a timer, never from a stream callback.
    pub fn needs_reopen(&self) -> bool {
        self.health.disconnected() || self.streams.as_ref().is_none_or(Streams::disconnected)
    }

    /// Closes the streams (if any) and opens them again with fresh rings for the pump.
    pub fn reopen(&mut self) -> Result<()> {
        self.streams = None;
        let (streams, rings) = uplink_android::audio::open(&self.health)?;
        self.streams = Some(streams);
        if self.swap.try_send(rings).is_err() {
            tracing::warn!("pump has not taken the previous rings yet");
        }
        Ok(())
    }

    pub fn running(&self) -> bool {
        self.tasks.all_running()
    }

    /// Mutes the microphone: captured audio is dropped rather than sent. Returns the new state.
    pub fn toggle_mute(&self) -> bool {
        let muted = !self.muted.load(Ordering::Relaxed);
        self.muted.store(muted, Ordering::Relaxed);
        muted
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Samples the callbacks moved, and samples lost at the rings, since the last call.
    pub fn taken_counts(&self) -> VoiceCounts {
        let take = |counter: &std::sync::atomic::AtomicU64| counter.swap(0, Ordering::Relaxed);
        VoiceCounts {
            captured: take(&self.health.captured_samples),
            played: take(&self.health.played_samples),
            capture_lost: take(&self.health.captured),
            playback_lost: take(&self.health.played),
        }
    }
}

/// Encodes whatever the microphone has produced and plays out whatever the peer sent.
async fn pump(
    mut swap: mpsc::Receiver<Rings>,
    mut sender: AudioSender,
    mut receiver: AudioReceiver,
    muted: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    let start = Instant::now();
    let mut tick = tokio::time::interval(FRAME_DURATION);
    let mut playout: Pcm = [0; FRAME_SAMPLES];
    let mut rings: Option<Rings> = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            fresh = swap.recv() => match fresh {
                Some(fresh) => {
                    tracing::info!("voice rings swapped");
                    rings = Some(fresh);
                }
                None => break,
            },
            _ = tick.tick() => {}
        }
        let Some(Rings { microphone, speaker }) = &mut rings else { continue };
        while microphone.slots() >= FRAME_SAMPLES {
            let mut frame: Pcm = [0; FRAME_SAMPLES];
            for sample in &mut frame {
                *sample = microphone.pop().unwrap_or_default();
            }
            let capture_micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
            // Muted: the ring is still drained, so unmuting doesn't play back stale audio.
            if muted.load(Ordering::Relaxed) {
                continue;
            }
            if let Err(e) = sender.send(&frame, capture_micros) {
                tracing::error!("voice send: {e}");
                return;
            }
        }
        match receiver.next(&mut playout) {
            // A full ring means playback stalled; dropping a whole frame keeps them aligned.
            Ok(_) if speaker.slots() >= FRAME_SAMPLES => {
                for &sample in &playout {
                    if speaker.push(sample).is_err() {
                        break;
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::error!("voice playout: {e}");
                return;
            }
        }
    }
    tracing::info!("voice stopped");
}
