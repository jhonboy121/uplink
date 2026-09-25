//! Call video: camera → encoder → peer, and peer → decoder → remote view. Each codec is driven by
//! a tokio task from its async-mode events; dropping [`CallVideo`] cancels both and waits for them.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use anyhow::{Context, Result};
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{Image, ImageFormat, ImageReader};
use ndk::native_window::NativeWindow;
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use uplink_android::codec::{Avc, Decoder, Encoder, Event, Events, VideoConfig};
use uplink_android::preview::TURNS_PER_REVOLUTION;
use uplink_core::media::{Frame, MediaStats, VideoSender};

use crate::tasks::Tasks;

const REMOTE_MAX_IMAGES: i32 = 4;
const KEYFRAME_QUEUE: usize = 1;
/// One change of quality at a time; a second waits for the task to take the first.
const SWAP_QUEUE: usize = 1;

/// The video half of a call's [`uplink_core::media::MediaSession`].
pub struct VideoParts {
    pub sender: VideoSender,
    pub incoming: mpsc::Receiver<Frame>,
    pub keyframe_requests: mpsc::Receiver<()>,
    pub stats: Arc<MediaStats>,
}

/// Field order is drop order: tasks stop first, then the shown image, then its reader.
pub struct CallVideo {
    tasks: Tasks,
    /// Our own ask for a keyframe: the camera coming back on after being off.
    keyframe: mpsc::Sender<()>,
    swap: mpsc::Sender<(Encoder, Events)>,
    /// What rate control last set, in bits a second; the task gives each change to the encoder.
    bitrate: watch::Sender<i32>,
    pub shown: Option<Image>,
    pub remote: ImageReader,
    encoder_window: NativeWindow,
    local_turns: Arc<AtomicU8>,
    remote_turns: Arc<AtomicU8>,
    pub stats: Arc<MediaStats>,
}

impl CallVideo {
    /// Starts both codecs; `on_remote_frame` runs whenever a decoded frame is ready to show.
    pub fn start(
        parts: VideoParts,
        avc: &Avc,
        video: VideoConfig,
        runtime: Handle,
        on_remote_frame: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let VideoParts { sender, incoming: incoming_video, keyframe_requests, stats } = parts;
        let (local_turns, remote_turns) = (Arc::<AtomicU8>::default(), Arc::<AtomicU8>::default());
        // Three things that can fail differently on a device we do not have. Named, because
        // "call video: ErrorUnknown" costs a day and a round trip to someone else's phone.
        let mut remote = ImageReader::new_with_usage(
            video.width,
            video.height,
            ImageFormat::PRIVATE,
            HardwareBufferUsage::GPU_SAMPLED_IMAGE,
            REMOTE_MAX_IMAGES,
        )
        .context("the reader the remote picture is decoded into")?;
        remote.set_image_listener(Box::new(move |_| on_remote_frame()))?;
        let window = remote.window().context("the reader's surface")?;
        let (decoder, decoder_events) =
            Decoder::new(avc, video.width, video.height, &window).context("the decoder")?;
        let (encoder, encoder_events) = Encoder::new(avc, video).context("the encoder")?;
        let encoder_window = encoder.window().clone();

        let mut tasks = Tasks::new(runtime);
        let cancel = tasks.cancel_token();
        tasks.spawn(decode(decoder, decoder_events, incoming_video, Arc::clone(&remote_turns), cancel.clone()));
        let (keyframe, ours) = mpsc::channel(KEYFRAME_QUEUE);
        let asks = Asks { theirs: keyframe_requests, ours };
        let (swap, swaps) = mpsc::channel(SWAP_QUEUE);
        let (bitrate, bitrates) = watch::channel(video.bitrate);
        let changes = Changes { swaps, bitrates };
        tasks.spawn(encode(encoder, encoder_events, sender, asks, changes, Arc::clone(&local_turns), cancel));
        Ok(Self { tasks, keyframe, swap, bitrate, shown: None, remote, encoder_window, local_turns, remote_turns, stats })
    }

    /// Sends from here on at another size, rate or bitrate: a new encoder replaces the old one
    /// under the same sender. The camera has to be pointed at [`Self::encoder_window`] again.
    pub fn reconfigure(&mut self, avc: &Avc, video: VideoConfig) -> Result<()> {
        let (encoder, events) = Encoder::new(avc, video).context("the new encoder")?;
        let window = encoder.window().clone();
        self.swap.try_send((encoder, events)).map_err(|_| anyhow::anyhow!("the encoder task is not taking a new encoder"))?;
        // Already the new encoder's; said again only so a change still on its way is not
        // applied on top of it.
        self.bitrate.send_replace(video.bitrate);
        self.encoder_window = window;
        Ok(())
    }

    /// The same picture at `bps` from here on: the running encoder changes over, with no new one
    /// and no keyframe.
    pub fn set_bitrate(&self, bps: i32) {
        self.bitrate.send_replace(bps);
    }

    /// The next frame is a keyframe, so the peer's decoder has something to start again from.
    pub fn request_keyframe(&self) {
        if self.keyframe.try_send(()).is_err() {
            tracing::debug!("keyframe already asked for");
        }
    }

    /// The surface the camera feeds for the peer.
    pub const fn encoder_window(&self) -> &NativeWindow {
        &self.encoder_window
    }

    /// Orientation sent along with our frames.
    pub fn set_local_turns(&self, turns: i32) {
        self.local_turns.store(quarter_turns(turns), Ordering::Relaxed);
    }

    /// Whether both codec tasks are still going (a codec error ends its task).
    pub fn codecs_running(&self) -> bool {
        self.tasks.all_running()
    }

    /// Orientation of the latest decoded remote frame.
    pub fn remote_turns(&self) -> i32 {
        i32::from(self.remote_turns.load(Ordering::Relaxed))
    }
}

fn quarter_turns(turns: i32) -> u8 {
    u8::try_from(turns.rem_euclid(TURNS_PER_REVOLUTION)).unwrap_or_default()
}

/// Logs a codec event that needs no action; returns false for a fatal error.
fn routine(event: &Event, codec: &str) -> bool {
    match event {
        Event::FormatChanged(format) => tracing::info!(codec, format, "output format"),
        Event::Error { detail, fatal } => {
            tracing::error!(codec, fatal, "{detail}");
            return !fatal;
        }
        Event::InputAvailable(_) | Event::OutputAvailable(..) => {}
    }
    true
}

/// Who can ask the encoder for a keyframe: the peer, and this side.
struct Asks {
    theirs: mpsc::Receiver<()>,
    ours: mpsc::Receiver<()>,
}

/// What rate control changes in the running task: a new encoder for another step, handed over
/// so the call's sender, and its sequence numbers, carry on; or the bitrate of the one it has.
struct Changes {
    swaps: mpsc::Receiver<(Encoder, Events)>,
    bitrates: watch::Receiver<i32>,
}

async fn encode(
    mut encoder: Encoder,
    mut events: Events,
    mut sender: VideoSender,
    mut asks: Asks,
    mut changes: Changes,
    turns: Arc<AtomicU8>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            // The old one stops here; the new one's first frame is a keyframe by nature.
            Some((fresh, fresh_events)) = changes.swaps.recv() => {
                encoder = fresh;
                events = fresh_events;
                tracing::info!("encoder swapped");
            }
            Ok(()) = changes.bitrates.changed() => {
                let bps = *changes.bitrates.borrow_and_update();
                if let Err(e) = encoder.set_bitrate(bps) {
                    tracing::warn!(bps, "encoder bitrate: {e}");
                }
            }
            Some(()) = asks.theirs.recv() => {
                match encoder.request_keyframe() {
                    Ok(()) => tracing::debug!("keyframe requested by peer"),
                    Err(e) => tracing::warn!("keyframe request: {e}"),
                }
            }
            Some(()) = asks.ours.recv() => {
                if let Err(e) = encoder.request_keyframe() {
                    tracing::warn!("keyframe request: {e}");
                }
            }
            event = events.recv() => match event {
                Some(Event::OutputAvailable(index, info)) => match encoder.take_output(index, &info) {
                    Ok(Some(packet)) => sender.send(Frame {
                        capture_micros: u64::try_from(packet.presentation_micros).unwrap_or_default(),
                        keyframe: packet.keyframe,
                        config: false,
                        turns: turns.load(Ordering::Relaxed),
                        data: packet.data,
                    }),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!("encoder output: {e}");
                        break;
                    }
                },
                Some(event) => {
                    if !routine(&event, "encoder") {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    tracing::info!("encoder stopped");
}

async fn decode(
    decoder: Decoder,
    mut events: Events,
    mut incoming: mpsc::Receiver<Frame>,
    turns: Arc<AtomicU8>,
    cancel: CancellationToken,
) {
    // Frames are only taken while a codec input is free; otherwise they back up in uplink-core,
    // which drops, resyncs and asks the peer for a keyframe.
    let mut free_inputs = VecDeque::new();
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            event = events.recv() => match event {
                Some(Event::InputAvailable(index)) => free_inputs.push_back(index),
                Some(Event::OutputAvailable(index, info)) => {
                    // Anything that is not a picture is released without being drawn; drawing it
                    // blanks the surface.
                    if let Err(e) = decoder.release(index, Decoder::is_picture(&info)) {
                        tracing::error!("decoder output: {e}");
                        break;
                    }
                }
                Some(event) => {
                    if !routine(&event, "decoder") {
                        break;
                    }
                }
                None => break,
            },
            frame = incoming.recv(), if !free_inputs.is_empty() => {
                let (Some(frame), Some(index)) = (frame, free_inputs.pop_front()) else { break };
                turns.store(frame.turns, Ordering::Relaxed);
                if let Err(e) = decoder.queue(index, &frame.data, frame.capture_micros, frame.keyframe) {
                    tracing::error!("decode: {e}");
                    break;
                }
            }
        }
    }
    tracing::info!("decoder stopped");
}
