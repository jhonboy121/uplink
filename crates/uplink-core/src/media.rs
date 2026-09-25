//! Video over QUIC: one unidirectional stream per encoded frame (design in docs/plan.md).
//!
//! Frames are reliable individually but never block each other. The sender resets frames that
//! miss [`FRAME_DEADLINE`], and ones QUIC has not delivered a round trip after that: handed over is
//! not delivered, and on a narrow path stale frames would otherwise hold the link and the peer's
//! stream credit for seconds while every new frame waits behind them. The receiver's [`Sequencer`]
//! delivers in order, and after a gap drops frames until the next keyframe while asking the peer
//! for one.
//!
//! Everything here follows the call's [`Link`] rather than one connection: a call that drops and
//! rejoins carries on over the new connection with the same codecs, counters and sequence numbers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use iroh::Endpoint;
use iroh::endpoint::{Connection, RecvStream, SendStream, StoppedError, VarInt};
use tokio::runtime::Handle;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::Instant;

use crate::audio::{self, AudioReceiver, AudioSender};
use crate::protocol::{FrameHeader, StreamHeader, StreamKind};
use crate::quality::{AudioCounts, Quality, VideoCounts};
use crate::{Error, protocol, telemetry};

/// Longest a frame may take to arrive before it is useless for live playback.
const FRAME_DEADLINE: Duration = Duration::from_millis(500);
/// How long a missing frame is waited for before it counts as lost.
const REORDER_WAIT: Duration = Duration::from_millis(150);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const MAX_FRAMES_IN_FLIGHT: usize = 8;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const INCOMING_FRAME_QUEUE: usize = 8;
const KEYFRAME_REQUEST_QUEUE: usize = 1;
/// Highest QUIC stream priority wins; video yields to signalling and (later) audio.
const VIDEO_PRIORITY: i32 = 0;
const STALE_FRAME: VarInt = VarInt::from_u32(0);
/// How often the link is read: arrivals for a stall, and the path's round trip and losses.
const LINK_CHECK: Duration = Duration::from_millis(500);
/// Nothing at all from the peer for this long is a call that has stopped, not a quiet one: QUIC
/// keeps a call's connection busy at least every [`crate::node::KEEP_ALIVE`], muted or not.
const STALL_AFTER: Duration = Duration::from_secs(2);

/// The connection a call's media currently runs over. Replaced when a dropped call rejoins; the
/// call ends, and everything following it stops, when the sending side is dropped.
pub(crate) type Link = watch::Receiver<Connection>;

/// An encoded video frame as produced by the encoder / consumed by the decoder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub capture_micros: u64,
    pub keyframe: bool,
    /// Codec configuration (e.g. H.264 SPS/PPS), sent ahead of the first keyframe.
    pub config: bool,
    /// Quarter turns the receiver applies to show the picture upright.
    pub turns: u8,
    pub data: Vec<u8>,
}

/// How a call is getting there. A relayed call and a direct one behave nothing alike — the same
/// pair of phones has run at 113 ms direct and 618 ms relayed with twelve-second excursions — and
/// nothing on screen says which you have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Route {
    #[default]
    Unknown,
    Direct,
    Relay,
}

impl Route {
    const fn code(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Direct => 1,
            Self::Relay => 2,
        }
    }

    const fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Direct,
            2 => Self::Relay,
            _ => Self::Unknown,
        }
    }
}

/// Media counters for one call; read by telemetry.
#[derive(Debug, Default)]
pub struct MediaStats {
    /// The path QUIC currently sends on, as a [`Route`] code. Written by telemetry each interval
    /// and read by the UI, which is the only way the call screen can say which one it has.
    route: AtomicU8,
    /// Nothing has arrived from the peer for [`STALL_AFTER`]: what "Reconnecting…" is shown from,
    /// well before the connection itself would be given up on.
    stalled: AtomicBool,
    /// The selected path's smoothed round trip in milliseconds, read twice a second; zero before
    /// the first reading. What the weak pill watches climb, and rate control too.
    pub rtt_ms: AtomicU64,
    /// The connection's packets lost and UDP datagrams sent, as last read: gauges of the current
    /// connection, which start again from zero on a rejoined one.
    pub lost_packets: AtomicU64,
    pub datagrams_sent: AtomicU64,
    /// The bitrate our encoder was last set to, in kbps, by rate control; zero on a voice call.
    pub video_kbps: AtomicU64,
    /// Times rate control changed the picture's step, down or up.
    pub step_changes: AtomicU64,
    /// Times the connection was lost mid-call, and times the call was rejoined on a new one.
    pub drops: AtomicU64,
    pub rejoins: AtomicU64,
    pub frames_sent: AtomicU64,
    /// Media payload each way, video frames and audio packets both: what a call carried, as the
    /// log reports it, without QUIC's own overhead.
    pub bytes_sent: AtomicU64,
    /// Not sent: too many frames already in flight.
    pub frames_dropped_congested: AtomicU64,
    /// Sent but reset after missing the deadline.
    pub frames_late: AtomicU64,
    /// The late ones whose stream never opened: the peer had given no credit for another.
    pub frames_unopened: AtomicU64,
    pub frames_received: AtomicU64,
    pub bytes_received: AtomicU64,
    /// Received but useless: older than already delivered frames, or after a gap.
    pub frames_dropped_received: AtomicU64,
    pub keyframe_requests_sent: AtomicU64,
    pub keyframe_requests_received: AtomicU64,
    pub audio_sent: AtomicU64,
    /// Not sent: the datagram send buffer was full.
    pub audio_send_dropped: AtomicU64,
    pub audio_received: AtomicU64,
    /// Arrived after its playout time (or while playback stalled).
    pub audio_late: AtomicU64,
    /// Lost packets rebuilt from the next packet's FEC.
    pub audio_fec_recovered: AtomicU64,
    /// Lost packets filled by Opus concealment.
    pub audio_concealed: AtomicU64,
    /// Buffered packets skipped to keep latency bounded.
    pub audio_skipped: AtomicU64,
    /// How rates spread across the call, sampled by telemetry each interval. The counts are
    /// filled in by [`Self::summary`] when someone asks.
    pub(crate) quality: parking_lot::Mutex<Quality>,
}

impl MediaStats {
    pub(crate) fn count(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

    /// How the call went so far: the sampled spreads, and every count as it stands now. Read
    /// once at the end of a call, for the log.
    pub fn summary(&self) -> Quality {
        let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let mut quality = *self.quality.lock();
        quality.video = VideoCounts {
            sent: count(&self.frames_sent),
            received: count(&self.frames_received),
            late: count(&self.frames_late),
            congested: count(&self.frames_dropped_congested),
            discarded: count(&self.frames_dropped_received),
            keyframe_asks_sent: count(&self.keyframe_requests_sent),
            keyframe_asks_received: count(&self.keyframe_requests_received),
        };
        quality.audio = AudioCounts {
            sent: count(&self.audio_sent),
            received: count(&self.audio_received),
            not_sent: count(&self.audio_send_dropped),
            late: count(&self.audio_late),
            rebuilt: count(&self.audio_fec_recovered),
            concealed: count(&self.audio_concealed),
        };
        quality.step_changes = u32::try_from(count(&self.step_changes)).unwrap_or(u32::MAX);
        quality.drops = u32::try_from(count(&self.drops)).unwrap_or(u32::MAX);
        quality.rejoins = u32::try_from(count(&self.rejoins)).unwrap_or(u32::MAX);
        quality
    }

    pub fn route(&self) -> Route {
        Route::from_code(self.route.load(Ordering::Relaxed))
    }

    pub(crate) fn set_route(&self, route: Route) {
        self.route.store(route.code(), Ordering::Relaxed);
    }

    pub fn stalled(&self) -> bool {
        self.stalled.load(Ordering::Relaxed)
    }
}

/// Sends encoded frames; usable from any thread (e.g. an encoder callback).
#[derive(Debug)]
pub struct VideoSender {
    link: Link,
    runtime: Handle,
    in_flight: Arc<Semaphore>,
    next_sequence: u64,
    stats: Arc<MediaStats>,
}

impl VideoSender {
    /// Queues `frame` without blocking. A frame dropped for congestion still consumes a sequence
    /// number, so the receiver sees the gap and asks for a keyframe.
    pub fn send(&mut self, frame: Frame) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            MediaStats::count(&self.stats.frames_dropped_congested, 1);
            return;
        };
        let (connection, stats) = (self.link.borrow().clone(), Arc::clone(&self.stats));
        self.runtime.spawn(async move {
            let queued = Instant::now();
            let bytes = u64::try_from(frame.data.len()).unwrap_or(u64::MAX);
            let sent = send_frame(&connection, sequence, frame, &stats).await;
            // The permit bounds frames waiting to be handed to QUIC; one being delivered holds
            // none, or a long round trip alone would leave no room for the next frame.
            drop(permit);
            let delivered = match sent {
                Ok(stream) => delivered(stream, queued, &stats).await,
                Err(e) => Err(e),
            };
            match delivered {
                Ok(()) => {
                    MediaStats::count(&stats.frames_sent, 1);
                    MediaStats::count(&stats.bytes_sent, bytes);
                }
                Err(Error::FrameLate) => MediaStats::count(&stats.frames_late, 1),
                Err(e) => tracing::debug!(sequence, "frame not sent: {e}"),
            }
        });
    }
}

/// Opens the frame's stream and hands it all to QUIC, which is not yet delivering it.
async fn send_frame(
    connection: &Connection,
    sequence: u64,
    frame: Frame,
    stats: &MediaStats,
) -> Result<SendStream, Error> {
    let header = StreamHeader {
        kind: Some(StreamKind::Video(FrameHeader {
            sequence,
            capture_micros: frame.capture_micros,
            keyframe: frame.keyframe,
            config: frame.config,
            turns: u32::from(frame.turns),
        })),
    };
    let mut stream = tokio::time::timeout(FRAME_DEADLINE, connection.open_uni()).await.map_err(|_| {
        MediaStats::count(&stats.frames_unopened, 1);
        Error::FrameLate
    })??;
    stream.set_priority(VIDEO_PRIORITY)?;
    match tokio::time::timeout(FRAME_DEADLINE, write_frame(&mut stream, &header, &frame.data)).await {
        Ok(written) => {
            written?;
            stream.finish()?;
            Ok(stream)
        }
        Err(_) => {
            stream.reset(STALE_FRAME)?;
            Err(Error::FrameLate)
        }
    }
}

/// Waits for the peer to have all of a finished frame: until [`FRAME_DEADLINE`] after it was
/// queued, and a round trip more for the acknowledgement to come back. Past that it is reset.
async fn delivered(stream: SendStream, queued: Instant, stats: &MediaStats) -> Result<(), Error> {
    let round_trip = Duration::from_millis(stats.rtt_ms.load(Ordering::Relaxed));
    match tokio::time::timeout_at(queued + FRAME_DEADLINE + round_trip, stream.stopped()).await {
        Ok(Ok(None)) => Ok(()),
        // Their reader gave up on it: it arrived too late to play.
        Ok(Ok(Some(_))) => Err(Error::FrameLate),
        // The connection went: whether the frame made it is the rejoin's business, not a count.
        Ok(Err(StoppedError::ConnectionLost(e))) => Err(e.into()),
        Ok(Err(StoppedError::ZeroRttRejected)) => Err(Error::Protocol("a frame stream on rejected 0-RTT")),
        Err(_) => {
            let mut stream = stream;
            if stream.reset(STALE_FRAME).is_err() {
                tracing::debug!("stale frame stream already closed");
            }
            Err(Error::FrameLate)
        }
    }
}

async fn write_frame(stream: &mut SendStream, header: &StreamHeader, data: &[u8]) -> Result<(), Error> {
    protocol::write_message(stream, header).await?;
    stream.write_all(data).await?;
    Ok(())
}

/// Media endpoints of a connected call, handed to the app with [`crate::node::Event::Connected`].
pub struct MediaSession {
    pub video: VideoSender,
    pub incoming_video: mpsc::Receiver<Frame>,
    /// The peer asked for a keyframe; the encoder should produce one soon.
    pub keyframe_requests: mpsc::Receiver<()>,
    pub audio: AudioSender,
    pub incoming_audio: AudioReceiver,
    pub stats: Arc<MediaStats>,
}

/// Written out, not derived: a session holds the QUIC connection and the codecs' buffers, and
/// deriving this put ten kilobytes of quinn internals into the log every time a call connected —
/// half the file, for an event whose interesting part is that it happened.
impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaSession")
    }
}

/// Call-task side of the media plumbing. Dropping it ends the media: everything following the
/// link stops once its sender is gone.
pub(crate) struct MediaLinks {
    /// Our receiver lost a reference frame: ask the peer for a keyframe.
    pub request_keyframe: mpsc::Receiver<()>,
    /// The peer asked us for a keyframe.
    pub keyframe_requested: mpsc::Sender<()>,
    pub stats: Arc<MediaStats>,
    link: watch::Sender<Connection>,
    endpoint: Endpoint,
}

impl MediaLinks {
    /// Carries the call on over `connection`, after the one before it was lost. The first frame
    /// on it is a keyframe: whatever the peer's decoder last had is no use to it now.
    pub fn rejoin(&self, connection: &Connection) {
        MediaStats::count(&self.stats.rejoins, 1);
        self.link.send_replace(connection.clone());
        tokio::spawn(telemetry::run(connection.clone(), self.endpoint.clone(), Arc::clone(&self.stats)));
        if self.keyframe_requested.try_send(()).is_err() {
            tracing::debug!("keyframe already asked for");
        }
    }
}

/// `endpoint` is for telemetry, which asks it what each side could offer for a direct path.
pub(crate) fn start(connection: &Connection, endpoint: &Endpoint) -> Result<(MediaSession, MediaLinks), Error> {
    let stats = Arc::<MediaStats>::default();
    let (link, following) = watch::channel(connection.clone());
    let (audio, incoming_audio) = audio::start(&following, &stats)?;
    let (incoming_tx, incoming_video) = mpsc::channel(INCOMING_FRAME_QUEUE);
    let (request_tx, request_keyframe) = mpsc::channel(KEYFRAME_REQUEST_QUEUE);
    let (keyframe_requested, keyframe_requests) = mpsc::channel(KEYFRAME_REQUEST_QUEUE);
    tokio::spawn(receive_video(following.clone(), incoming_tx, request_tx, Arc::clone(&stats)));
    tokio::spawn(watch_link(following.clone(), Arc::clone(&stats)));
    tokio::spawn(telemetry::run(connection.clone(), endpoint.clone(), Arc::clone(&stats)));
    let video = VideoSender {
        link: following,
        runtime: Handle::current(),
        in_flight: Arc::new(Semaphore::new(MAX_FRAMES_IN_FLIGHT)),
        next_sequence: 0,
        stats: Arc::clone(&stats),
    };
    let session =
        MediaSession { video, incoming_video, keyframe_requests, audio, incoming_audio, stats: Arc::clone(&stats) };
    let links = MediaLinks { request_keyframe, keyframe_requested, stats, link, endpoint: endpoint.clone() };
    Ok((session, links))
}

/// Says whether anything at all is arriving from the peer, and keeps the path's gauges fresh for
/// the weak pill and rate control, for as long as the call lasts. Telemetry's lines come only
/// every few seconds.
async fn watch_link(mut link: Link, stats: Arc<MediaStats>) {
    let mut tick = tokio::time::interval(LINK_CHECK);
    let (mut last, mut since) = (None, Instant::now());
    loop {
        tick.tick().await;
        // An error here is the call over, not a new connection.
        if link.has_changed().is_err() {
            break;
        }
        let connection = link.borrow_and_update().clone();
        let link_stats = connection.stats();
        if let Some(path) = telemetry::selected_path(&connection) {
            stats.rtt_ms.store(u64::try_from(path.rtt.as_millis()).unwrap_or(u64::MAX), Ordering::Relaxed);
        }
        stats.lost_packets.store(link_stats.lost_packets, Ordering::Relaxed);
        stats.datagrams_sent.store(link_stats.udp_tx.datagrams, Ordering::Relaxed);
        let arrived = (connection.stable_id(), link_stats.udp_rx.datagrams);
        let now = Instant::now();
        if last != Some(arrived) {
            (last, since) = (Some(arrived), now);
        }
        let stalled = now.duration_since(since) >= STALL_AFTER;
        if stats.stalled.swap(stalled, Ordering::Relaxed) != stalled {
            tracing::info!(stalled, "arrivals");
        }
    }
}

/// Accepts one stream per frame and feeds the sequencer, over each connection the call has.
async fn receive_video(
    mut link: Link,
    deliver: mpsc::Sender<Frame>,
    request_keyframe: mpsc::Sender<()>,
    stats: Arc<MediaStats>,
) {
    let (arrived_tx, mut arrived) = mpsc::channel(MAX_FRAMES_IN_FLIGHT);
    let mut sequencer = Sequencer::new(Instant::now());
    let mut connection = link.borrow_and_update().clone();
    // The connection is gone and the next has not come yet.
    let mut lost = false;
    loop {
        let wake = sequencer.next_deadline();
        let outputs = tokio::select! {
            changed = link.changed() => {
                if changed.is_err() {
                    break;
                }
                connection = link.borrow_and_update().clone();
                lost = false;
                continue;
            }
            stream = connection.accept_uni(), if !lost => {
                match stream {
                    Ok(stream) => {
                        tokio::spawn(read_frame(stream, arrived_tx.clone(), Arc::clone(&stats)));
                    }
                    Err(_) => lost = true,
                }
                continue;
            }
            Some((sequence, frame)) = arrived.recv() => sequencer.push(sequence, frame, Instant::now()),
            () = sleep_until(wake) => sequencer.expire(Instant::now()),
        };
        let mut outputs = std::collections::VecDeque::from(outputs);
        while let Some(output) = outputs.pop_front() {
            match output {
                Output::Deliver(frame) => {
                    if deliver.try_send(frame).is_err() {
                        // The decoder missed a frame it may depend on.
                        MediaStats::count(&stats.frames_dropped_received, 1);
                        outputs.extend(sequencer.resync(Instant::now()));
                    }
                }
                Output::Dropped(count) => MediaStats::count(&stats.frames_dropped_received, count),
                Output::RequestKeyframe => {
                    // A request already queued covers this one.
                    if request_keyframe.try_send(()).is_ok() {
                        MediaStats::count(&stats.keyframe_requests_sent, 1);
                    }
                }
            }
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn read_frame(mut stream: RecvStream, arrived: mpsc::Sender<(u64, Frame)>, stats: Arc<MediaStats>) {
    let read = async {
        let header: StreamHeader = protocol::read_message(&mut stream).await?;
        // The only kind of stream so far; any other is from a newer build and not for this one.
        let Some(StreamKind::Video(header)) = header.kind else {
            return Err(Error::Protocol("a stream of a kind this build does not know"));
        };
        let data = stream.read_to_end(MAX_FRAME_BYTES).await?;
        Ok::<_, Error>((header, data))
    };
    match tokio::time::timeout(FRAME_DEADLINE, read).await {
        Ok(Ok((header, data))) => {
            MediaStats::count(&stats.frames_received, 1);
            MediaStats::count(&stats.bytes_received, u64::try_from(data.len()).unwrap_or(u64::MAX));
            let frame = Frame {
                capture_micros: header.capture_micros,
                keyframe: header.keyframe,
                config: header.config,
                // Quarter turns, so 0–3; anything else is a sender's bug, shown upright.
                turns: u8::try_from(header.turns).unwrap_or_default(),
                data,
            };
            if arrived.send((header.sequence, frame)).await.is_err() {
                tracing::debug!("frame arrived after the call ended");
            }
        }
        Ok(Err(e)) => tracing::debug!("frame stream failed: {e}"),
        Err(_) => {
            MediaStats::count(&stats.frames_dropped_received, 1);
            if stream.stop(STALE_FRAME).is_err() {
                tracing::debug!("late frame stream already closed");
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Output {
    Deliver(Frame),
    Dropped(u64),
    RequestKeyframe,
}

/// Orders frames and recovers from gaps; pure logic driven by explicit instants.
struct Sequencer {
    next: u64,
    /// After a gap (and at start) only a keyframe or codec config can resume decoding.
    awaiting_keyframe: bool,
    pending: BTreeMap<u64, (Instant, Frame)>,
    last_request: Option<Instant>,
}

impl Sequencer {
    const fn new(now: Instant) -> Self {
        // The encoder starts with a keyframe, so count the start as a fresh request.
        Self { next: 0, awaiting_keyframe: true, pending: BTreeMap::new(), last_request: Some(now) }
    }

    fn push(&mut self, sequence: u64, frame: Frame, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if sequence < self.next {
            out.push(Output::Dropped(1));
        } else if frame.keyframe || frame.config {
            // Decodable on its own: anything older is no longer needed.
            let newer = self.pending.split_off(&sequence);
            out.push(Output::Dropped(u64::try_from(self.pending.len()).unwrap_or(u64::MAX)));
            self.pending = newer;
            self.awaiting_keyframe = false;
            self.next = sequence + 1;
            out.push(Output::Deliver(frame));
            self.flush(&mut out);
        } else if self.awaiting_keyframe {
            out.push(Output::Dropped(1));
            self.maybe_request(now, &mut out);
        } else if sequence == self.next {
            self.next += 1;
            out.push(Output::Deliver(frame));
            self.flush(&mut out);
        } else {
            self.pending.insert(sequence, (now, frame));
        }
        out.retain(|o| *o != Output::Dropped(0));
        out
    }

    /// Called when [`Self::next_deadline`] passes: a frame waited for too long is lost.
    fn expire(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        let gap = self
            .pending
            .values()
            .next()
            .is_some_and(|(arrived, _)| now.duration_since(*arrived) >= REORDER_WAIT);
        if gap {
            let dropped = u64::try_from(self.pending.len()).unwrap_or(u64::MAX);
            self.pending.clear();
            self.awaiting_keyframe = true;
            out.push(Output::Dropped(dropped));
        }
        if self.awaiting_keyframe {
            self.maybe_request(now, &mut out);
        }
        out
    }

    /// Something downstream lost a frame: wait for the next keyframe.
    fn resync(&mut self, now: Instant) -> Vec<Output> {
        let mut out = vec![Output::Dropped(u64::try_from(self.pending.len()).unwrap_or(u64::MAX))];
        self.pending.clear();
        self.awaiting_keyframe = true;
        self.maybe_request(now, &mut out);
        out.retain(|o| *o != Output::Dropped(0));
        out
    }

    fn next_deadline(&self) -> Option<Instant> {
        let gap = self.pending.values().next().map(|(arrived, _)| *arrived + REORDER_WAIT);
        let retry = self
            .awaiting_keyframe
            .then(|| self.last_request.map_or_else(Instant::now, |last| last + KEYFRAME_REQUEST_INTERVAL));
        match (gap, retry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn flush(&mut self, out: &mut Vec<Output>) {
        while let Some(entry) = self.pending.first_entry() {
            if *entry.key() != self.next {
                break;
            }
            let (_, frame) = entry.remove();
            self.next += 1;
            out.push(Output::Deliver(frame));
        }
    }

    fn maybe_request(&mut self, now: Instant, out: &mut Vec<Output>) {
        if self.last_request.is_none_or(|last| now.duration_since(last) >= KEYFRAME_REQUEST_INTERVAL) {
            self.last_request = Some(now);
            out.push(Output::RequestKeyframe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: u64, keyframe: bool) -> Frame {
        Frame { capture_micros: id, keyframe, config: false, turns: 0, data: vec![0; 1] }
    }

    fn delivered(outputs: &[Output]) -> Vec<u64> {
        outputs
            .iter()
            .filter_map(|o| match o {
                Output::Deliver(frame) => Some(frame.capture_micros),
                _ => None,
            })
            .collect()
    }

    fn requested(outputs: &[Output]) -> bool {
        outputs.contains(&Output::RequestKeyframe)
    }

    /// A sequencer that already delivered keyframe 0.
    fn started(now: Instant) -> Sequencer {
        let mut sequencer = Sequencer::new(now);
        assert_eq!(delivered(&sequencer.push(0, frame(0, true), now)), [0]);
        sequencer
    }

    #[test]
    fn delivers_in_order_after_a_keyframe() {
        let now = Instant::now();
        let mut sequencer = started(now);
        assert_eq!(delivered(&sequencer.push(1, frame(1, false), now)), [1]);
        assert_eq!(delivered(&sequencer.push(2, frame(2, false), now)), [2]);
    }

    #[test]
    fn nothing_decodable_before_the_first_keyframe() {
        let now = Instant::now();
        let mut sequencer = Sequencer::new(now);
        let out = sequencer.push(0, frame(0, false), now);
        assert_eq!(out, [Output::Dropped(1)], "no request yet: the start counts as one");
        let later = now + KEYFRAME_REQUEST_INTERVAL;
        assert!(requested(&sequencer.push(1, frame(1, false), later)));
        assert_eq!(delivered(&sequencer.push(2, frame(2, true), later)), [2]);
    }

    #[test]
    fn reorders_frames_that_arrive_early() {
        let now = Instant::now();
        let mut sequencer = started(now);
        assert!(sequencer.push(2, frame(2, false), now).is_empty());
        assert_eq!(sequencer.next_deadline(), Some(now + REORDER_WAIT));
        assert_eq!(delivered(&sequencer.push(1, frame(1, false), now)), [1, 2]);
    }

    #[test]
    fn gap_drops_until_keyframe_and_requests_one() {
        let now = Instant::now();
        let mut sequencer = started(now);
        sequencer.push(2, frame(2, false), now);
        let late = now + REORDER_WAIT.max(KEYFRAME_REQUEST_INTERVAL);
        let out = sequencer.expire(late);
        assert!(out.contains(&Output::Dropped(1)));
        assert!(requested(&out));
        assert_eq!(sequencer.push(3, frame(3, false), late), [Output::Dropped(1)], "rate limited");
        assert_eq!(delivered(&sequencer.push(4, frame(4, true), late)), [4]);
        assert_eq!(delivered(&sequencer.push(5, frame(5, false), late)), [5]);
    }

    #[test]
    fn stale_frames_are_dropped() {
        let now = Instant::now();
        let mut sequencer = started(now);
        sequencer.push(1, frame(1, false), now);
        assert_eq!(sequencer.push(1, frame(1, false), now), [Output::Dropped(1)]);
    }

    #[test]
    fn keyframe_supersedes_pending_frames() {
        let now = Instant::now();
        let mut sequencer = started(now);
        sequencer.push(3, frame(3, false), now);
        sequencer.push(5, frame(5, false), now);
        let out = sequencer.push(4, frame(4, true), now);
        assert!(out.contains(&Output::Dropped(1)), "frame 3 is older than the keyframe");
        assert_eq!(delivered(&out), [4, 5]);
    }

    #[test]
    fn codec_config_resumes_like_a_keyframe() {
        let now = Instant::now();
        let mut sequencer = Sequencer::new(now);
        let config = Frame { config: true, ..frame(0, false) };
        assert_eq!(delivered(&sequencer.push(0, config, now)), [0]);
        assert_eq!(delivered(&sequencer.push(1, frame(1, false), now)), [1]);
    }

    #[test]
    fn resync_waits_for_a_keyframe() {
        let now = Instant::now();
        let mut sequencer = started(now);
        sequencer.push(2, frame(2, false), now);
        let later = now + KEYFRAME_REQUEST_INTERVAL;
        let out = sequencer.resync(later);
        assert!(out.contains(&Output::Dropped(1)) && requested(&out));
        assert_eq!(sequencer.push(3, frame(3, false), later), [Output::Dropped(1)]);
        assert_eq!(delivered(&sequencer.push(4, frame(4, true), later)), [4]);
    }
}
