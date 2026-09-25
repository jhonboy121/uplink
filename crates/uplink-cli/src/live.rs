//! A phone's call media, emulated for the TUI. The clip is decoded, scaled to the call's step and
//! encoded live at rate control's bitrate, as the app's camera feeds MediaCodec; without a clip, a
//! moving test pattern stands in for the camera. The voice goes unless muted or held, the peer's
//! media is taken, counted and optionally recorded.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use h264::{Decoder, Encoder, EncoderConfig, I420, YuvRef};
use tokio::sync::{mpsc, watch};
use uplink_core::audio::{AudioReceiver, AudioSender, FRAME_DURATION, FRAME_SAMPLES, Pcm};
use uplink_core::media::{Frame, MediaSession, VideoSender};
use uplink_core::preset::Preset;

use crate::clip::annex_b;
use crate::clip::{Clip, VideoTrack};
use crate::record::{FRAGMENT_INTERVAL, Recorder};

const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// How often a paused picture (camera off, held, voice call) looks at the plan again.
const IDLE_POLL: Duration = Duration::from_millis(20);
/// Frames between IDRs: none but the first and the peer's asks, like the app's encoder.
const KEYFRAME_INTERVAL: Option<u32> = None;
const BITS_PER_BYTE: f64 = 8.0;
const BITS_PER_KBIT: f64 = 1000.0;
const BYTES_PER_KIB: u32 = 1024;
const MICROS_PER_SECOND: u64 = 1_000_000;
/// Test pattern: how far its bars move a frame, and their width.
const PATTERN_SPEED: u32 = 4;
const PATTERN_BAR: u32 = 64;
const CHROMA_NEUTRAL: u8 = 128;

/// What the call wants of its media now; the TUI's controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub step: Preset,
    pub kbps: u32,
    /// The picture goes: a video call, the camera on, not held.
    pub video: bool,
    /// The voice goes: the mic on, not held.
    pub voice: bool,
    /// Their voice is played (recorded): not held.
    pub playout: bool,
    pub voice_bps: i32,
}

/// What our encoder did over the last second.
#[derive(Clone, Copy, Debug, Default)]
pub struct Encoding {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub kbps: f64,
    pub qp: u8,
    pub keyframes: u32,
    /// Output ticks the encoder could not keep up with.
    pub behind: u32,
}

/// What the peer sent over the last second.
#[derive(Clone, Copy, Debug, Default)]
pub struct Received {
    pub fps: f64,
    pub kbps: f64,
    pub keyframes: u32,
    pub largest_kib: u32,
    pub turns: u8,
    /// Their picture's size, from the last SPS they sent.
    pub size: Option<(u32, u32)>,
}

/// A call's running media; dropping it stops everything.
pub struct Live {
    pub plan: watch::Sender<Plan>,
    pub encoding: watch::Receiver<Encoding>,
    pub received: watch::Receiver<Received>,
    stop: Arc<AtomicBool>,
    tasks: tokio::task::JoinSet<()>,
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tasks.abort_all();
    }
}

/// The clip, read and decoded in order, a picture per sample.
struct ClipSource {
    file: File,
    track: VideoTrack,
    decoder: Decoder,
    next: usize,
    /// Pictures decoded on this pass through the clip; none in a whole pass is a clip we can't play.
    decoded: usize,
}

impl ClipSource {
    fn new(file: File, track: VideoTrack) -> Self {
        Self { file, track, decoder: Decoder::new(), next: 0, decoded: 0 }
    }

    /// Time between the clip's pictures.
    fn interval(&self) -> Duration {
        let samples = &self.track.samples;
        let span = samples
            .last()
            .map_or(0, |s| s.decode_time)
            .saturating_sub(samples.first().map_or(0, |s| s.decode_time));
        let frames = u32::try_from(samples.len().saturating_sub(1)).unwrap_or(u32::MAX).max(1);
        let micros = span
            .saturating_mul(MICROS_PER_SECOND)
            .checked_div(u64::from(self.track.timescale))
            .unwrap_or_default();
        Duration::from_micros(micros) / frames
    }

    /// One pass through the clip: the loop both the picture and the voice keep to.
    fn length(&self) -> Duration {
        self.interval() * u32::try_from(self.track.samples.len()).unwrap_or(u32::MAX)
    }

    fn sample(&self, index: usize) -> Result<Option<Vec<u8>>> {
        let Some(sample) = self.track.samples.get(index) else { return Ok(None) };
        let mut data = vec![0; usize::try_from(sample.size)?];
        self.file.read_exact_at(&mut data, sample.offset)?;
        annex_b(&data, self.track.length_size, sample.sync.then_some(self.track.config.as_slice())).map(Some)
    }

    /// The next picture at its own size; `None` once this pass is over, after which the next
    /// call starts the clip over from its first sample, a keyframe.
    fn next_picture(&mut self) -> Result<Option<I420>> {
        loop {
            if let Some(data) = self.sample(self.next)? {
                self.next += 1;
                if let Some(picture) = self.decoder.decode(&data).map_err(|e| anyhow!("decoding the clip: {e}"))? {
                    self.decoded += 1;
                    return Ok(Some(owned(&picture.yuv())));
                }
            } else if let Some(picture) = self.decoder.flush() {
                self.decoded += 1;
                return Ok(Some(owned(&picture.yuv())));
            } else {
                ensure!(self.decoded > 0, "the clip decoded to no pictures");
                (self.decoder, self.next, self.decoded) = (Decoder::new(), 0, 0);
                return Ok(None);
            }
        }
    }
}

/// A decoded picture copied out of the decoder, which reuses its buffers.
fn owned(picture: &YuvRef<'_>) -> I420 {
    let mut out = I420::new(picture.width, picture.height);
    let copy = |to: &mut [u8], from: &[u8], stride: usize, width: u32| {
        let width = usize::try_from(width).unwrap_or_default();
        for (to, from) in to.chunks_exact_mut(width).zip(from.chunks(stride)) {
            to.copy_from_slice(from.get(..width).unwrap_or_default());
        }
    };
    copy(&mut out.y, picture.y, picture.y_stride, picture.width);
    copy(&mut out.u, picture.u, picture.c_stride, picture.chroma_width());
    copy(&mut out.v, picture.v, picture.c_stride, picture.chroma_width());
    out
}

/// Bars sliding across a luma ramp, with moving chroma: texture and motion for the encoder.
fn pattern(out: &mut I420, frame: u32) {
    let (width, height) = (out.width, out.height);
    let shift = frame.wrapping_mul(PATTERN_SPEED);
    for (row, line) in out.y.chunks_exact_mut(usize::try_from(width).unwrap_or(usize::MAX)).enumerate() {
        let row = u32::try_from(row).unwrap_or_default();
        for (column, luma) in line.iter_mut().enumerate() {
            let column = u32::try_from(column).unwrap_or_default();
            let bar = (column.wrapping_add(shift) / PATTERN_BAR).is_multiple_of(2);
            let ramp = (row * u32::from(u8::MAX)).checked_div(height).unwrap_or_default();
            *luma = u8::try_from(if bar { ramp } else { u32::from(u8::MAX) - ramp }).unwrap_or(u8::MAX);
        }
    }
    let tint = u8::try_from(shift % u32::from(u8::MAX)).unwrap_or_default();
    out.u.fill(tint);
    out.v.fill(CHROMA_NEUTRAL.wrapping_add(tint));
}

fn encoder(step: Preset, kbps: u32, slices: u32) -> Result<Encoder> {
    let target = step.video();
    Encoder::with_config(EncoderConfig {
        width: target.width,
        height: target.height,
        fps: target.fps,
        bitrate_kbps: kbps,
        keyframe_interval: KEYFRAME_INTERVAL,
        slices,
    })
    .map_err(|e| anyhow!("encoder for {step:?}: {e}"))
}

/// Runs `work` on a thread of its own, logging through this one's subscriber.
fn spawn(name: &'static str, work: impl FnOnce() -> Result<()> + Send + 'static) {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    std::thread::spawn(move || {
        let _log = tracing::dispatcher::set_default(&dispatch);
        if let Err(e) = work() {
            tracing::warn!("{name} stopped: {e:#}");
        }
    });
}

/// Starts a call's media. `clip` of `None` sends the test pattern and silence.
pub fn start(media: MediaSession, clip: Option<Clip>, recorder: Option<Recorder>, plan: Plan) -> Live {
    let MediaSession { video, incoming_video, keyframe_requests, audio, incoming_audio, .. } = media;
    // The one clock the picture's and the voice's positions in the clip, and their capture
    // times, are read from, so the two cannot drift apart.
    let epoch = Instant::now();
    let (plan_tx, plan_rx) = watch::channel(plan);
    let (encoding_tx, encoding) = watch::channel(Encoding::default());
    let (received_tx, received) = watch::channel(Received::default());
    let stop = Arc::new(AtomicBool::new(false));
    let keyframe = Arc::new(AtomicBool::new(false));
    let (pictures, voice, length, turns) = match clip {
        Some(Clip { file, video, voice }) => {
            let turns = video.turns;
            let clip = ClipSource::new(file, video);
            let length = clip.length();
            let (pictures_tx, pictures) = watch::channel(None);
            let stop = Arc::clone(&stop);
            spawn("the clip", move || decode_clip(clip, epoch, &stop, &pictures_tx));
            (Some(pictures), voice, Some(length), turns)
        }
        None => (None, Vec::new(), None, 0),
    };
    {
        let (plan, stop, keyframe) = (plan_rx.clone(), Arc::clone(&stop), Arc::clone(&keyframe));
        let camera = Camera { pictures, turns, epoch };
        spawn("our picture", move || send_video(camera, video, &plan, &stop, &keyframe, &encoding_tx));
    }
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(send_voice(voice, length, epoch, audio, plan_rx.clone()));
    tasks.spawn(receive(incoming_video, incoming_audio, keyframe_requests, keyframe, recorder, plan_rx, received_tx));
    Live { plan: plan_tx, encoding, received, stop, tasks }
}

/// Decodes the clip in real time on its own thread, each pass starting on the loop's clock, and
/// hands on the newest picture. The picture keeps its place in the clip whatever the encoder does.
fn decode_clip(
    mut clip: ClipSource,
    epoch: Instant,
    stop: &AtomicBool,
    pictures: &watch::Sender<Option<Arc<I420>>>,
) -> Result<()> {
    let (interval, length) = (clip.interval(), clip.length());
    let (mut pass, mut index) = (epoch, 0u32);
    while !stop.load(Ordering::Relaxed) {
        let due = pass + interval * index;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        match clip.next_picture()? {
            Some(picture) => {
                pictures.send_replace(Some(Arc::new(picture)));
                index += 1;
            }
            None => (pass, index) = (pass + length, 0),
        }
    }
    Ok(())
}

/// What stands in for the phone's camera: the clip's newest picture, or the pattern.
struct Camera {
    pictures: Option<watch::Receiver<Option<Arc<I420>>>>,
    turns: u8,
    epoch: Instant,
}

/// The encoder's loop, on its own thread: the camera's newest picture at the step's size and
/// rate, until the call's media is dropped.
fn send_video(
    mut camera: Camera,
    mut sender: VideoSender,
    plan: &watch::Receiver<Plan>,
    stop: &AtomicBool,
    keyframe: &AtomicBool,
    report: &watch::Sender<Encoding>,
) -> Result<()> {
    // A slice per core: the encoder's unit of parallelism.
    let slices = std::thread::available_parallelism().map_or(1, |cores| u32::try_from(cores.get()).unwrap_or(1));
    let mut now_plan = *plan.borrow();
    let mut coder = encoder(now_plan.step, now_plan.kbps, slices)?;
    let mut picture = I420::new(now_plan.step.video().width, now_plan.step.video().height);
    // Nothing to encode until the camera has given a picture at this size.
    let mut fresh = false;
    let mut pattern_frame = 0u32;
    let mut next_out = Instant::now();
    let (mut window, mut since) = (Encoding::default(), Instant::now());
    let (mut frames, mut bytes) = (0u32, 0u32);
    let mut paused = !now_plan.video;
    while !stop.load(Ordering::Relaxed) {
        let wanted = *plan.borrow();
        if wanted.step != now_plan.step {
            // A new step is a new encoder, as on the phone: its first frame is a keyframe.
            coder = encoder(wanted.step, wanted.kbps, slices)?;
            picture = I420::new(wanted.step.video().width, wanted.step.video().height);
            fresh = false;
            if let Some(pictures) = &mut camera.pictures {
                pictures.mark_changed();
            }
            tracing::info!(step = ?wanted.step, kbps = wanted.kbps, "our picture steps");
        } else if wanted.kbps != now_plan.kbps {
            coder.set_bitrate(wanted.kbps);
        }
        now_plan = wanted;
        if !now_plan.video {
            paused = true;
            std::thread::sleep(IDLE_POLL);
            next_out = Instant::now();
            continue;
        }
        // Back from off or hold: the peer's decoder has nothing current to predict from.
        let asked = keyframe.swap(false, Ordering::Relaxed);
        if std::mem::take(&mut paused) || asked {
            coder.force_idr();
        }
        match &mut camera.pictures {
            Some(pictures) => {
                if pictures.has_changed().unwrap_or(false)
                    && let Some(newest) = pictures.borrow_and_update().clone()
                {
                    h264::scale_i420(&I420::as_ref(&newest), &mut picture);
                    fresh = true;
                }
            }
            None => {
                pattern(&mut picture, pattern_frame);
                pattern_frame = pattern_frame.wrapping_add(1);
                fresh = true;
            }
        }
        if fresh {
            let capture_micros = u64::try_from(camera.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
            let encoded = coder.encode(&picture.as_ref()).map_err(|e| anyhow!("encoding: {e}"))?;
            frames += 1;
            window.keyframes += u32::from(encoded.keyframe);
            window.qp = encoded.qp;
            bytes = bytes.saturating_add(u32::try_from(encoded.data.len()).unwrap_or(u32::MAX));
            sender.send(Frame {
                capture_micros,
                keyframe: encoded.keyframe,
                config: false,
                turns: camera.turns,
                data: encoded.data,
            });
        }
        if since.elapsed() >= REPORT_INTERVAL {
            let target = now_plan.step.video();
            report.send_replace(Encoding {
                width: target.width,
                height: target.height,
                fps: per_second(frames, since),
                kbps: per_second(bytes, since) * BITS_PER_BYTE / BITS_PER_KBIT,
                ..window
            });
            (window, since, frames, bytes) = (Encoding::default(), Instant::now(), 0, 0);
        }
        next_out += Duration::from_secs(1) / now_plan.step.video().fps.max(1);
        let now = Instant::now();
        if next_out < now {
            window.behind += 1;
            next_out = now;
        } else {
            std::thread::sleep(next_out - now);
        }
    }
    Ok(())
}

/// The clip's audio as 20 ms Opus frames, each taken from where the loop's clock says the clip
/// is, so it stays with the picture; silence without one, or past the end of its audio. Muted or
/// held, the frames are skipped, as the app drains its microphone without sending.
async fn send_voice(
    voice: Vec<i16>,
    length: Option<Duration>,
    epoch: Instant,
    mut audio: AudioSender,
    plan: watch::Receiver<Plan>,
) {
    let silence: Pcm = [0; FRAME_SAMPLES];
    let (frames, _) = voice.as_chunks::<FRAME_SAMPLES>();
    let epoch = tokio::time::Instant::from_std(epoch);
    let mut tick = tokio::time::interval_at(epoch, FRAME_DURATION);
    let mut told = 0;
    loop {
        let at = tick.tick().await;
        let since = at.duration_since(epoch);
        let frame = length
            .and_then(|length| {
                let into = since.as_nanos().checked_rem(length.as_nanos())?;
                frames.get(usize::try_from(into / FRAME_DURATION.as_nanos()).ok()?)
            })
            .unwrap_or(&silence);
        let Plan { voice, voice_bps, .. } = *plan.borrow();
        if voice_bps != told {
            match audio.set_bitrate(voice_bps) {
                Ok(()) => tracing::info!(bps = voice_bps, "voice bitrate"),
                Err(e) => tracing::warn!(bps = voice_bps, "voice bitrate: {e}"),
            }
            told = voice_bps;
        }
        if !voice {
            continue;
        }
        let capture_micros = u64::try_from(since.as_micros()).unwrap_or(u64::MAX);
        if let Err(e) = audio.send(frame, capture_micros) {
            tracing::warn!("our voice stopped: {e:#}");
            return;
        }
    }
}

/// The picture size in the SPS at the head of a keyframe.
fn sps_size(data: &[u8]) -> Option<(u32, u32)> {
    h264::nal_units(data)
        .find(|nal| h264::nal::nal_type(nal) == Some(h264::nal::SPS))
        .and_then(h264::sps_dimensions)
}

/// Takes the peer's video, plays out their audio on a 20 ms clock (there is no speaker; playout
/// drives the jitter buffer and the recording) and forwards their keyframe asks to the encoder.
async fn receive(
    mut incoming_video: mpsc::Receiver<Frame>,
    mut incoming_audio: AudioReceiver,
    mut keyframe_requests: mpsc::Receiver<()>,
    keyframe: Arc<AtomicBool>,
    mut recorder: Option<Recorder>,
    plan: watch::Receiver<Plan>,
    report: watch::Sender<Received>,
) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + REPORT_INTERVAL, REPORT_INTERVAL);
    let mut flush = tokio::time::interval_at(tokio::time::Instant::now() + FRAGMENT_INTERVAL, FRAGMENT_INTERVAL);
    let mut playout = tokio::time::interval(FRAME_DURATION);
    let mut pcm: Pcm = [0; FRAME_SAMPLES];
    let (mut window, mut since) = (Received::default(), Instant::now());
    let (mut frames, mut bytes) = (0u32, 0u32);
    let mut size = None;
    loop {
        tokio::select! {
            frame = incoming_video.recv() => match frame {
                Some(frame) => {
                    let length = u32::try_from(frame.data.len()).unwrap_or(u32::MAX);
                    frames += 1;
                    bytes = bytes.saturating_add(length);
                    window.keyframes += u32::from(frame.keyframe);
                    window.largest_kib = window.largest_kib.max(length / BYTES_PER_KIB);
                    window.turns = frame.turns;
                    if frame.keyframe || frame.config {
                        size = sps_size(&frame.data).or(size);
                    }
                    if let Some(recorder) = &mut recorder
                        && let Err(e) = recorder.video(&frame)
                    {
                        tracing::warn!("recording video: {e:#}");
                    }
                }
                None => break,
            },
            _ = playout.tick() => match incoming_audio.next(&mut pcm) {
                // Held, what they send is still taken off the network, just not played.
                Ok(Some((sequence, packet))) if plan.borrow().playout => {
                    if let Some(recorder) = &mut recorder {
                        recorder.audio(sequence, packet);
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("their voice: {e:#}"),
            },
            Some(()) = keyframe_requests.recv() => keyframe.store(true, Ordering::Relaxed),
            _ = flush.tick() => {
                if let Some(recorder) = &mut recorder
                    && let Err(e) = recorder.flush()
                {
                    tracing::warn!("recording: {e:#}");
                }
            }
            _ = tick.tick() => {
                report.send_replace(Received {
                    fps: per_second(frames, since),
                    kbps: per_second(bytes, since) * BITS_PER_BYTE / BITS_PER_KBIT,
                    size,
                    ..window
                });
                (window, since, frames, bytes) = (Received::default(), Instant::now(), 0, 0);
            }
        }
    }
}

fn per_second(count: u32, since: Instant) -> f64 {
    f64::from(count) / since.elapsed().as_secs_f64().max(f64::EPSILON)
}

/// Fails early on a clip we cannot read or decode, before any call.
pub fn check_clip(path: &Path) -> Result<()> {
    let Clip { file, video, .. } = Clip::open(path)?;
    ClipSource::new(file, video).next_picture()?.context("the clip has no pictures")?;
    Ok(())
}
