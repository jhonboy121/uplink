//! uplink Android entry point: calls with camera video through the zero-copy GL path, with
//! in-app diagnostics (previous exits + previous log) since there is no adb.

mod audio;
mod clock;
mod core;
mod tasks;
mod ui;
mod video;
mod view;

use std::cell::RefCell;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::native_window::{FrameRateCompatibility, NativeWindow};
use rustc_hash::FxHashSet;
use slint::android::AndroidApp;
use slint::android::android_activity::{MainEvent, PollEvent};
use slint::{ComponentHandle, Model as _, ModelRc, RenderingState, Timer, TimerMode, VecModel};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{Dispatch, Level};
use uplink_android::camera::{Camera, Facing, Intent};
use uplink_android::codec::{Avc, VideoConfig};
use uplink_android::platform::{CallScreen, Permission, Platform, PlatformEvent, RouteKind, TelecomState, Text};
use uplink_android::preview::{Frame, Preview};
use uplink_android::{cpu, log};
use uplink_core::audio::{AudioReceiver, AudioSender};
use uplink_core::calls::{self, CallId, CallLog};
use uplink_core::card;
use uplink_core::contacts::Contacts;
use uplink_core::elapsed;
use uplink_core::health::{Health, Weak};
use uplink_core::logs;
use uplink_core::media::{MediaSession, MediaStats, Route};
use uplink_core::node::{Behind, Command, EndReason, Event, MediaState, Mode, RelayView, Steer};
use uplink_core::preset::{Network as PresetNetwork, Preset};
use uplink_core::qr;
use uplink_core::rate::{Change, Rate, Reading};
use uplink_core::reach::Reach as CoreReach;
use uplink_core::relays::{self, Choice, Ranking, Source};
use uplink_core::settings::{self, Settings};
use uplink_core::{EndpointId, RelayUrl};

use crate::clock::LocalClock;
use crate::core::Core;

use crate::audio::CallAudio;
use crate::ui::{
    AddError, AddProblem, App, Appearance, CallCapture, CallState, Confirm, Grant, Language, Mismatch, PermissionItem,
    QualitySheet, RelayItem, Say, Screen, Screenshots, Theme, Toast,
};
use crate::video::{CallVideo, VideoParts};
use crate::view::{maybe, none, one, toast};

const LOG_TAG: &CStr = c"uplink";
/// Baked in at build time (`just log=debug apk`). iroh's path events (opened, selected,
/// abandoned) and the selector's RTTs say why a call ran over the path it did; they only fire when
/// paths change, so they are always on.
const LOG_FILTER: &str = match option_env!("UPLINK_LOG") {
    Some(filter) => filter,
    None => "info,iroh::_events::path=debug,iroh::socket::biased_rtt_path_selector=trace",
};
const FALLBACK_DATA_DIR: &str = "/data/local/tmp";
/// Stats ticks per line written to the log. The counters are read every second either way.
const STATS_LOG_EVERY: u32 = 5;

const CAPTURE_WIDTH: i32 = 1280;
const CAPTURE_HEIGHT: i32 = 720;
const CAPTURE_FPS: i32 = 30;
const READER_MAX_IMAGES: i32 = 4;
const QR_PIXELS: usize = 512;
/// What the shared files are called — the key never changes, so one card is rewritten rather than
/// a new one each time. Their captions and titles are Android's resources, in the app's language.
const SHARE_FILE: &str = "identity.png";
const DIAGNOSTICS_FILE: &str = "uplink-logs.tar.gz";
/// The bundled translation's folder under `crates/uplink/lang`; English is the markup itself.
const ARABIC: &str = "ar";
// Scanning: CPU-readable frames, big enough to read a code held up to the camera.
const SCAN_WIDTH: i32 = 960;
const SCAN_HEIGHT: i32 = 720;
const SCAN_MAX_IMAGES: i32 = 2;
const KEYFRAME_INTERVAL_SECS: i32 = 2;
const BPS_PER_KBPS: u32 = 1000;

/// What the encoder is set up with for a preset: its size, rate and bitrate cap.
const fn video_config(preset: Preset) -> VideoConfig {
    let video = preset.video();
    VideoConfig {
        width: video.width.cast_signed(),
        height: video.height.cast_signed(),
        fps: video.fps.cast_signed(),
        bitrate: bps(video.kbps),
        keyframe_interval_secs: KEYFRAME_INTERVAL_SECS,
    }
}

/// The encoder counts in bits a second, the presets and rate control in kbps.
const fn bps(kbps: u32) -> i32 {
    (kbps * BPS_PER_KBPS).cast_signed()
}
const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// The markup's "Android has not said which output yet".
const NO_OUTPUT: i32 = -1;
/// How long after our own network changes a stalled call is put down to us rather than them: a
/// switch between wifi and mobile data takes about a second, and the relay a moment more.
const UNSETTLED: Duration = Duration::from_secs(5);
/// How long a call waits for the other phone to say its screen is blocked, once asked, before
/// the call screen says their app cannot: a build that knows the ask answers within a second.
const CAPTURE_ANSWER: Duration = Duration::from_secs(5);

/// Who wants this phone's screen kept from screenshots and recordings, and what the other side
/// of the call said about theirs.
#[derive(Clone, Copy, Default)]
struct Capture {
    /// Settings: always, on this phone.
    block: bool,
    /// Settings: every call asks the other phone to.
    ask: bool,
    /// This call's other side asks us to.
    peer_asked: bool,
    /// This call's other side says its screen is kept from capture.
    peer_blocked: bool,
}

impl Capture {
    /// From the two saved flags. Asking their phone always blocks this one too: a setting saved
    /// when the two were separate switches, asking without blocking, opens as both phones.
    const fn saved(block: bool, ask: bool) -> Self {
        Self { block: block || ask, ask, peer_asked: false, peer_blocked: false }
    }

    const fn choice(self) -> Screenshots {
        if self.ask {
            Screenshots::BothPhones
        } else if self.block {
            Screenshots::ThisPhone
        } else {
            Screenshots::Allowed
        }
    }

    const fn choose(&mut self, choice: Screenshots) {
        self.block = !matches!(choice, Screenshots::Allowed);
        self.ask = matches!(choice, Screenshots::BothPhones);
    }

    const fn secure(self) -> bool {
        self.block || self.peer_asked
    }

    /// What the call screen says: whose phone this call cannot be captured on, or that we asked
    /// an app that has not said it can.
    fn shown(self, connected_for: Option<Duration>) -> CallCapture {
        match (self.ask && self.peer_blocked, self.peer_asked) {
            (true, true) => CallCapture::Both,
            (true, false) => CallCapture::Theirs,
            (false, true) => CallCapture::Ours,
            (false, false) if self.ask && connected_for.is_some_and(|connected| connected >= CAPTURE_ANSWER) => {
                CallCapture::Unsupported
            }
            (false, false) => CallCapture::None,
        }
    }
}
/// How long a just-added contact shimmers: two sweeps and a bit, enough to find it and no more.
const FRESH_FOR: Duration = Duration::from_millis(3200);
const PERCENT: f64 = 100.0;
/// How long a call the network ended stays on screen to say so.
const LOST_LINGER: Duration = Duration::from_secs(4);

#[derive(Default)]
struct FrameStats {
    camera: AtomicU32,
    remote: AtomicU32,
    blits: AtomicU32,
    blit_micros: AtomicU64,
}

/// Seconds without an encoded frame before the other side is told our camera is off, rather than
/// left looking at the last one.
const CAMERA_QUIET_SECS: u32 = 2;
/// The first wait before reopening a camera again, doubling to the most while it keeps failing.
const CAMERA_RETRY_FIRST: Duration = Duration::from_secs(3);
const CAMERA_RETRY_MOST: Duration = Duration::from_secs(30);

/// Our camera in a video call, as the watchdog last saw it. A Huawei in the field paused the
/// camera for as long as the call was in picture-in-picture, whatever the foreground service
/// said, and once took it away outright; nothing brought it back but turning it off and on.
#[derive(Default)]
struct CameraWatch {
    /// Seconds in a row without a frame.
    quiet: u32,
    /// The other side has been told our camera is off while it sends nothing.
    paused: bool,
    /// Not reopened again before this.
    retry_at: Option<Instant>,
    /// The wait after the next reopen.
    backoff: Duration,
    /// In picture-in-picture at the last look: coming back to full screen retries at once.
    pip: bool,
    /// The encoder's frame count at the last look.
    encoded: u64,
}

/// Field order is drop order: the shown image, then the camera, then the readers it feeds.
struct Session {
    shown: Option<Image>,
    camera: Camera,
    reader: ImageReader,
    /// Held only to keep the scan stream alive; its listener does the work.
    _scanner: Option<ImageReader>,
}

/// Field order is drop order: the camera stops feeding the encoder before the call goes.
struct State {
    ui: slint::Weak<App>,
    session: Option<Session>,
    audio: Option<CallAudio>,
    call: Option<CallVideo>,
    avc: Avc,
    runtime: Handle,
    facing: Facing,
    extra_turns: i32,
    mirror: bool,
    resume_camera: bool,
    camera_watch: CameraWatch,
    scanning: bool,
    /// One decode at a time; frames that arrive meanwhile are dropped.
    scan_busy: Arc<AtomicBool>,
    contacts: Contacts,
    log: CallLog,
    /// Contacts ticked for removal. Kept here rather than in the model, which is rebuilt whenever
    /// the list changes and would drop the ticks with it.
    selected: FxHashSet<EndpointId>,
    /// Calls ticked for removal, kept here for the same reason.
    selected_calls: FxHashSet<CallId>,
    /// For the on-screen timer only; the call log is the core's to write.
    connected_at: Option<Instant>,
    /// How the call in progress was placed or offered.
    mode: Mode,
    /// A voice call's video half, kept unstarted until both sides agree to switch to video.
    parked: Option<VideoParts>,
    /// Who the call in progress, or the one just lost, is with: what Call again dials.
    peer: Option<EndpointId>,
    /// Ours, as the other side is told it. Off stops the camera, and with it the encoder's input.
    camera_on: bool,
    /// The core is getting a dropped connection back.
    reconnecting: bool,
    /// The call as Telecom last said: held, muted by the system, and where the sound goes.
    telecom: TelecomState,
    /// What the screen and the small window's output button were last told, so each is told
    /// again only when it changes.
    chrome: Option<Chrome>,
    /// The quality steps this phone's camera and encoder manage, asked once per window.
    presets: Option<Vec<Preset>>,
    /// The step the call in progress may send at: its cap, for this network.
    sending: Option<Preset>,
    /// What the call's video sends within that cap, while the path cannot carry all of it.
    rate: Option<Rate>,
    /// The call's counters, voice or video: a voice call has no codecs to hold them.
    media: Option<Arc<MediaStats>>,
    /// The chip's answer, which also says whose side a stalled call is on.
    reach: CoreReach,
    /// When the platform last reported a network change: a call that stalls soon after is ours.
    network_moved: Option<Instant>,
    /// Judges the weak pill, afresh for each call.
    health: Health,
    /// This phone's own key, which is never anyone to call or save.
    me: EndpointId,
    /// The contact just added, shimmering in the People list until [`FRESH_FOR`] has passed.
    fresh: Option<EndpointId>,
    /// The phone's own time of day, for every time the app shows.
    clock: LocalClock,
    stats: Arc<FrameStats>,
    /// Commands to the endpoint, which this window borrows rather than owns — the endpoint
    /// belongs to the process and outlives every window it is shown in.
    core: Option<Arc<Core>>,
    settings: Settings,
    capture: Capture,
    /// The relays as the endpoint last reported them: the survey, the map, the home relay.
    relays: Option<RelayView>,
}

impl State {
    fn clear_frame(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_frame(slint::Image::default());
        }
    }

    /// Something worth knowing about later. It goes to the log file, which is the only copy
    /// anyone reads — there is no log on screen, because a log on screen never leaves the phone.
    fn status(&self, message: impl AsRef<str>) {
        tracing::info!("{}", message.as_ref());
    }

    /// Assumes the camera permission is granted (see [`request_camera`]).
    fn start_camera(&mut self) {
        self.session = None;
        // The last picture of whatever ran before would otherwise stay up until this camera's
        // first frame replaces it: a flash of the front camera before the scanner's back one.
        self.clear_frame();
        match self.open_session() {
            Ok(session) => {
                let camera = &session.camera;
                tracing::info!(
                    id = camera.id(),
                    facing = ?camera.facing(),
                    orientation = camera.sensor_orientation(),
                    intent = ?camera.intent(),
                    "camera started"
                );
                self.status(format!(
                    "camera {} ({:?}) sensor {}° · {CAPTURE_WIDTH}x{CAPTURE_HEIGHT}",
                    camera.id(),
                    camera.facing(),
                    camera.sensor_orientation()
                ));
                self.session = Some(session);
                self.sync_call_turns();
            }
            Err(e) => {
                tracing::error!("camera start: {e:#}");
                self.status(format!("camera start failed: {e:#}"));
            }
        }
    }

    fn open_session(&self) -> Result<Session> {
        let mut reader = ImageReader::new_with_usage(
            CAPTURE_WIDTH,
            CAPTURE_HEIGHT,
            ImageFormat::PRIVATE,
            HardwareBufferUsage::GPU_SAMPLED_IMAGE,
            READER_MAX_IMAGES,
        )?;
        let stats = Arc::clone(&self.stats);
        let ui = self.ui.clone();
        reader.set_image_listener(Box::new(move |_| {
            stats.camera.fetch_add(1, Ordering::Relaxed);
            // Fails only once the event loop has quit; nothing left to redraw then.
            let _ = ui.upgrade_in_event_loop(|ui| ui.window().request_redraw());
        }))?;
        let scanner = self.scanning.then(|| self.open_scanner()).transpose()?;
        let preview = reader.window()?;
        let scan_window = scanner.as_ref().map(ImageReader::window).transpose()?;
        let windows: Vec<&NativeWindow> = std::iter::once(&preview)
            .chain(self.call.as_ref().map(CallVideo::encoder_window))
            .chain(scan_window.as_ref())
            .collect();
        // Codes are held up in front of you: that is the back camera's job.
        let facing = if self.scanning { Facing::Back } else { self.facing };
        // The encoder's surface is one of the targets during a call, and the HAL has to be told.
        let intent = if self.call.is_some() { Intent::Record } else { Intent::Preview };
        // In a call the camera runs at the rate the preset sends; otherwise it is only a preview.
        let fps = match (&self.call, self.sending) {
            (Some(_), Some(preset)) => video_config(preset).fps,
            _ => CAPTURE_FPS,
        };
        let camera = Camera::open(facing, &windows, fps, intent)?;
        Ok(Session { shown: None, camera, reader, _scanner: scanner })
    }

    /// A small CPU-readable stream: QR needs the pixels, which the preview's GPU-only buffers
    /// never expose. Only the luma plane is read — that is already the greyscale a decoder wants.
    fn open_scanner(&self) -> Result<ImageReader> {
        let mut reader = ImageReader::new(SCAN_WIDTH, SCAN_HEIGHT, ImageFormat::YUV_420_888, SCAN_MAX_IMAGES)?;
        let (ui, runtime, busy, me) = (self.ui.clone(), self.runtime.clone(), Arc::clone(&self.scan_busy), self.me);
        reader.set_image_listener(Box::new(move |reader| {
            // The callback must stay quick and must never outlive the reader: it only copies the
            // frame, and decoding happens on a worker.
            match copy_luma(reader) {
                Err(e) => tracing::debug!("scan frame: {e:#}"),
                // A decode is still running; this frame goes in the bin.
                Ok(Some(_)) if busy.swap(true, Ordering::Relaxed) => {}
                Ok(Some(frame)) => {
                    let (ui, busy) = (ui.clone(), Arc::clone(&busy));
                    runtime.spawn_blocking(move || {
                        let found = qr::decode_luma(&frame.luma, frame.width, frame.height, frame.stride);
                        busy.store(false, Ordering::Relaxed);
                        // Nothing found is the normal case while the camera hunts, so it is
                        // silent; a code that is not ours is worth saying, once, and scanning
                        // carries on so the next code still gets a chance.
                        let Some(text) = found else { return };
                        let key = match peer_key(&text, &me) {
                            Ok(key) => key,
                            Err(unusable) => {
                                let _ = ui.upgrade_in_event_loop(move |ui| {
                                    if ui.get_toast().row_count() == 0 {
                                        toast(&ui, unusable.say(), "");
                                    }
                                });
                                return;
                            }
                        };
                        tracing::info!("scanned a key");
                        // `invoke_scan` runs the same handler the button does, so the camera
                        // rebuilds without this reader. The contacts live on the UI thread, so
                        // whether this key is already one of them is decided there.
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            ui.invoke_scan(false);
                            ui.invoke_key_found(key.to_string().into());
                        });
                    });
                }
                Ok(None) => {}
            }
        }))?;
        Ok(reader)
    }

    /// Tells the peer how to turn our frames upright; the self-view's mirror stays local.
    fn sync_call_turns(&self) {
        if let (Some(call), Some(session)) = (&self.call, &self.session) {
            call.set_local_turns(session.camera.upright_quarter_turns() + self.extra_turns);
        }
    }

    fn start_video(&mut self, parts: VideoParts) {
        let (stats, ui) = (Arc::clone(&self.stats), self.ui.clone());
        let on_remote_frame = move || {
            stats.remote.fetch_add(1, Ordering::Relaxed);
            // Fails only once the event loop has quit; nothing left to redraw then.
            let _ = ui.upgrade_in_event_loop(|ui| ui.window().request_redraw());
        };
        let cap = self.sending.unwrap_or(Preset::Balanced);
        match CallVideo::start(parts, &self.avc, video_config(cap), self.runtime.clone(), on_remote_frame) {
            Ok(call) => {
                self.pace_from(cap, &call.stats);
                self.call = Some(call);
            }
            Err(e) => {
                // Loud on both ends of the report: in the log with the reason, and on the call
                // screen, because the peer cannot tell a broken encoder from a still room.
                tracing::error!("call video: {e:#}");
                self.status(format!("call video failed: {e:#}"));
                self.video_failed();
            }
        }
    }

    /// Says on the call screen that the call runs on audio alone: without it, a call whose codecs
    /// never came up looks exactly like a peer who is sitting still.
    fn video_failed(&self) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_video_failed(true);
        }
    }

    /// Returns whether the microphone ended up muted.
    fn toggle_mic(&mut self) -> bool {
        self.audio.as_ref().is_some_and(CallAudio::toggle_mute)
    }

    /// Turns our camera off, which stops what the encoder is fed, or back on, starting with a
    /// keyframe. Returns whether it ended up on.
    fn toggle_camera(&mut self) -> bool {
        self.camera_on = !self.camera_on;
        if self.camera_on {
            self.start_camera();
            if let Some(call) = &self.call {
                call.request_keyframe();
            }
        } else {
            self.session = None;
        }
        self.camera_on
    }

    /// Once a second in a video call: reopens a camera its own callbacks say was taken away or
    /// failed, and pauses our picture for the other side while no frame goes out. Returns whether
    /// that pause changed, which they have to be told.
    ///
    /// Reopening goes by the callbacks alone; the frames only decide the pause, since a phone
    /// that stops the camera in picture-in-picture says nothing. They are the encoder's, which
    /// are what the other side gets: the preview's stop whenever the app is not drawing, while
    /// the camera goes on feeding the encoder.
    fn watch_camera(&mut self, pip: bool) -> bool {
        let Some(call) = self.call.as_ref().filter(|_| self.camera_on && !self.telecom.held) else {
            return std::mem::take(&mut self.camera_watch).paused;
        };
        let encoded = call.stats.frames_encoded.load(Ordering::Relaxed);
        let now = Instant::now();
        let watch = &mut self.camera_watch;
        let frames = encoded.saturating_sub(std::mem::replace(&mut watch.encoded, encoded));
        if std::mem::replace(&mut watch.pip, pip) && !pip {
            (watch.retry_at, watch.backoff) = (None, CAMERA_RETRY_FIRST);
        }
        if frames > 0 {
            (watch.quiet, watch.retry_at, watch.backoff) = (0, None, CAMERA_RETRY_FIRST);
        } else {
            watch.quiet = watch.quiet.saturating_add(1);
        }
        let error = self.session.as_ref().and_then(|session| session.camera.error());
        // A reopen that failed leaves no camera to call back, so it is tried again.
        let reopen_failed = self.session.is_none() && watch.retry_at.is_some();
        if (error.is_some() || reopen_failed) && watch.retry_at.is_none_or(|at| now >= at) {
            let wait = watch.backoff.max(CAMERA_RETRY_FIRST);
            (watch.retry_at, watch.backoff) = (Some(now + wait), (wait * 2).min(CAMERA_RETRY_MOST));
            tracing::warn!(error, reopen_failed, pip, "camera lost mid-call; reopening");
            self.start_camera();
            if let Some(call) = &self.call {
                call.request_keyframe();
            }
        }
        let watch = &mut self.camera_watch;
        let paused = watch.quiet >= CAMERA_QUIET_SECS;
        if paused == watch.paused {
            return false;
        }
        watch.paused = paused;
        tracing::info!(paused, pip, "our camera's picture");
        // Back: the other side's decoder starts again from a whole picture.
        if !paused && let Some(call) = &self.call {
            call.request_keyframe();
        }
        true
    }

    /// What the other side is told about our mic, our camera and our hold.
    fn media_state(&self) -> MediaState {
        let mic_off = self.audio.as_ref().is_some_and(CallAudio::muted);
        MediaState {
            mic_off,
            camera_off: !self.camera_on || self.camera_watch.paused,
            held: self.telecom.held,
            capture_asked: self.capture.ask,
            capture_blocked: self.capture.secure(),
        }
    }

    /// A phone call took this one over, or gave it back. The voice streams let go of the audio
    /// the other call has, and the camera stops; back, both return where the user left them,
    /// the camera with a keyframe.
    fn apply_hold(&mut self, held: bool) {
        self.health.ours_restarted();
        if let Some(audio) = &mut self.audio
            && let Err(e) = audio.set_held(held)
        {
            tracing::warn!(held, "voice streams on hold: {e:#}");
        }
        if held {
            self.session = None;
        } else if self.camera_on && self.call.is_some() {
            self.start_camera();
            if let Some(call) = &self.call {
                call.request_keyframe();
            }
        }
    }

    /// Time since the call connected.
    fn call_timer(&self) -> String {
        elapsed::timer(self.connected_at.map(|at| at.elapsed()).unwrap_or_default())
    }

    /// Starts the microphone and speaker; the call keeps running without them.
    fn start_audio(&mut self, sender: AudioSender, receiver: AudioReceiver) {
        let voice = self.sending.unwrap_or(Preset::Balanced).voice_bps();
        self.audio = Some(CallAudio::start(sender, receiver, self.runtime.clone(), voice));
    }

    /// Reopens voice streams that AAudio disconnected (re-routing, headphones). Runs on the UI
    /// thread's timer, which is what AAudio requires: never from a stream callback.
    fn recover_audio(&mut self) {
        let Some(audio) = &mut self.audio else { return };
        if !audio.needs_reopen() {
            return;
        }
        match audio.reopen() {
            Ok(()) => {
                // The gap this leaves in playout is ours, not their network's.
                self.health.ours_restarted();
                self.status("voice streams reopened");
            }
            Err(e) => tracing::warn!("reopening voice streams: {e:#}"),
        }
    }

    /// Rate control starts over at `cap`, all of it: a new call, or a new network or choice.
    fn pace_from(&mut self, cap: Preset, stats: &MediaStats) {
        let rate = Rate::new(cap, self.presets.as_deref().unwrap_or_default());
        stats.video_kbps.store(u64::from(rate.kbps()), Ordering::Relaxed);
        self.rate = Some(rate);
    }

    /// Rate control's second: the call's counters in, and the encoder changed over if they say so.
    fn pace(&mut self) {
        let (Some(rate), Some(media)) = (&mut self.rate, &self.media) else { return };
        let change = rate.sample(Reading::read(media, Instant::now()));
        let Some(call) = &mut self.call else { return };
        match change {
            Change::None => return,
            Change::Bitrate(kbps) => call.set_bitrate(bps(kbps)),
            Change::Step(step, kbps) => {
                tracing::info!(?step, kbps, "call picture steps");
                media.step_changes.fetch_add(1, Ordering::Relaxed);
                let video = VideoConfig { bitrate: bps(kbps), ..video_config(step) };
                if let Err(e) = call.reconfigure(&self.avc, video) {
                    tracing::warn!("stepping the call's picture: {e:#}");
                    return;
                }
                if self.session.is_some() {
                    self.start_camera();
                }
            }
        }
        if let (Some(rate), Some(media)) = (&self.rate, &self.media) {
            media.video_kbps.store(u64::from(rate.kbps()), Ordering::Relaxed);
        }
    }

    /// Stops the call's codecs, streams and camera. Only an open scanner keeps the camera,
    /// rebuilt without the encoder: a camera nothing shows costs battery, and Android holds an
    /// app streaming one at 60 Hz.
    fn end_call(&mut self) {
        self.rate = None;
        if self.call.is_none() && self.audio.is_none() {
            return;
        }
        let camera_was_running = self.session.take().is_some();
        self.camera_watch = CameraWatch::default();
        self.audio = None;
        self.call = None;
        self.parked = None;
        if camera_was_running && self.scanning {
            self.start_camera();
        } else {
            self.clear_frame();
        }
    }
}

/// One frame's luma plane, copied so the reader's buffer goes straight back to the camera.
struct ScanFrame {
    luma: Vec<u8>,
    width: usize,
    height: usize,
    stride: usize,
}

fn copy_luma(reader: &ImageReader) -> Result<Option<ScanFrame>> {
    const LUMA_PLANE: i32 = 0;
    let AcquireResult::Image(image) = reader.acquire_latest_image()? else { return Ok(None) };
    let stride = usize::try_from(image.plane_row_stride(LUMA_PLANE)?)?;
    let (width, height) = (usize::try_from(image.width()?)?, usize::try_from(image.height()?)?);
    Ok(Some(ScanFrame { luma: image.plane_data(LUMA_PLANE)?.to_vec(), width, height, stride }))
}

/// A key read from a code: named in the sheet if it is new, and said to be known if it is not —
/// before the sheet, not as an error after it, since no name typed there could fix it.
fn offer_key(state: &Rc<RefCell<State>>, ui: &App, key: EndpointId) {
    match with_contacts(state, |contacts| contacts.name_of(&key).map(str::to_owned)).flatten() {
        Some(name) => toast(ui, Say::AlreadySaved, name),
        None => ui.set_pending_key(one(key.to_string().into())),
    }
}

/// Asks for an image and reads the key out of it, for a code that arrived over chat.
fn pick_key(state: &Rc<RefCell<State>>, platform: &Rc<Platform>) {
    let (state, platform) = (Rc::clone(state), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        let picked = platform.pick_image().await;
        let Some(ui) = state.try_borrow().ok().and_then(|s| s.ui.upgrade()) else { return };
        match picked {
            Ok(Some(image)) => match qr::decode_luma(&image.pixels, image.width, image.height, image.width) {
                Some(text) => match peer_key(&text, &state.borrow().me) {
                    Ok(key) => offer_key(&state, &ui, key),
                    Err(unusable) => toast(&ui, unusable.say(), ""),
                },
                None => toast(&ui, Say::NoCode, ""),
            },
            Ok(None) => {}
            Err(e) => {
                tracing::error!("picking an image: {e}");
                toast(&ui, Say::ImageUnreadable, e);
            }
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning the image picker: {e}");
    }
}

/// Names whoever is on the other end, with the initial the avatar shows.
fn set_peer(ui: &App, name: &str) {
    ui.set_peer_name(name.into());
    ui.set_peer_initial(view::initial(name));
}

/// The relay page, from the store and the endpoint's last report. The ticks and the mode are the
/// store's, so the rows always show what the pilot will read; the latencies and what is in use
/// are the report's, or the stored ranking's before the first report arrives.
fn show_relays(state: &Rc<RefCell<State>>, ui: &App) {
    let s = state.borrow();
    let catalogue = relays::catalogue(&s.settings);
    let choice = Choice::load(&s.settings);
    let ranking = s.relays.as_ref().map_or_else(|| Ranking::load(&s.settings), |live| live.ranking.clone());
    let rows = |source| {
        let items: Vec<RelayItem> = catalogue
            .iter()
            .filter(|relay| relay.source == source)
            .map(|relay| view::relay_item(relay, &choice, &ranking, s.relays.as_ref()))
            .collect();
        view::list(items)
    };
    ui.set_uplink_relays(rows(Source::Uplink));
    ui.set_n0_relays(rows(Source::N0));
    ui.set_your_relays(rows(Source::Yours));
    ui.set_relays_auto(choice.auto);
    ui.set_relays_on(i32::try_from(choice.pool(&catalogue).len()).unwrap_or(i32::MAX));
    ui.set_relays_checked(view::checked(&ranking));
}

/// Tells the endpoint's pilot, which applies relay changes to the live endpoint.
fn steer_relays(state: &Rc<RefCell<State>>, steer: Steer) {
    let Some(core) = state.borrow().core.clone() else {
        tracing::warn!(?steer, "the endpoint is still starting");
        return;
    };
    if let Err(e) = core.calls().try_send(Command::Relays(steer)) {
        tracing::warn!(?steer, "relay pilot: {e}");
    }
}

/// The key as a QR image, and the share of its width the mark in the middle may cover. Only the
/// widening of the greyscale to the RGB Slint takes happens here; the code itself is core's.
fn qr_image(id: &EndpointId) -> Result<(slint::Image, f32)> {
    let matrix = qr::encode(&id.to_string())?;
    let (luma, side) = matrix.render(QR_PIXELS);
    let side = u32::try_from(side).unwrap_or_default();
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    for (pixel, value) in buffer.make_mut_slice().iter_mut().zip(luma) {
        *pixel = slint::Rgb8Pixel { r: value, g: value, b: value };
    }
    Ok((slint::Image::from_rgb8(buffer), ratio(matrix.logo(), matrix.framed())))
}

/// Hands the identity to the share sheet as a picture of its code. Drawn by core, written where
/// the app's content provider can serve it, and handed to Android by name.
async fn write_identity_card(data_dir: PathBuf, key: String, caption: String) -> Result<()> {
    let directory = Platform::share_dir(&data_dir);
    let card = card::identity(&key, &caption)?;
    tokio::fs::create_dir_all(&directory).await?;
    Ok(tokio::fs::write(directory.join(SHARE_FILE), card).await?)
}

/// Writes a file on the runtime, then offers it to the share sheet from the UI loop — which is
/// where the platform bridge lives, because it is an `Rc` and does not cross threads.
fn share_when_written(
    handle: &Handle,
    platform: &Rc<Platform>,
    ui: &slint::Weak<App>,
    file: &'static str,
    title: Text,
    trouble: Say,
    write: impl Future<Output = Result<()>> + Send + 'static,
) {
    let writing = handle.spawn(write);
    let (platform, weak) = (Rc::clone(platform), ui.clone());
    spawn_ui(async move {
        let shared = match writing.await {
            Ok(Ok(())) => platform
                .context()
                .text(title)
                .and_then(|title| platform.share_file(file, &title))
                .map_err(anyhow::Error::from),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::Error::from(e)),
        };
        if let (Err(e), Some(ui)) = (shared, weak.upgrade()) {
            tracing::error!("sharing {file}: {e:#}");
            toast(&ui, trouble, "");
        }
    });
}

/// Sends every kept log file as one archive. Reading a log off a phone screen is no way to
/// report anything, and a tester on another continent has no adb; this is how a fault gets here.
///
/// Packing runs on the tokio runtime, never on the UI thread: the set can be a hundred megabytes
/// before it compresses, and the tap that starts it must not freeze the phone.
async fn pack_diagnostics(data_dir: PathBuf) -> Result<()> {
    let out = Platform::share_dir(&data_dir).join(DIAGNOSTICS_FILE);
    logs::archive(&data_dir, log::LOG_STEM, logs::KEEP, &out).await?;
    let size = tokio::fs::metadata(&out).await.map(|file| file.len()).unwrap_or_default();
    tracing::info!(bytes = size, "diagnostics packed");
    Ok(())
}

/// A count of modules as a share of the whole drawing.
fn ratio(part: usize, whole: usize) -> f32 {
    part as f32 / whole.max(1) as f32
}

/// A contact was just added: open People on it, and let it shimmer for a moment.
fn show_added(state: &Rc<RefCell<State>>, ui: &App, peer: EndpointId) {
    with_state(state, |s| s.fresh = Some(peer));
    show_contacts(state, ui);
    ui.set_screen(Screen::People);
    let (state, weak) = (Rc::clone(state), ui.as_weak());
    Timer::single_shot(FRESH_FOR, move || {
        // Only if it is still the same one: a second add in the meantime has its own timer.
        let cleared = with_state_value(&state, |s| {
            let same = s.fresh == Some(peer);
            if same {
                s.fresh = None;
            }
            same
        });
        if cleared == Some(true)
            && let Some(ui) = weak.upgrade()
        {
            show_contacts(&state, &ui);
        }
    });
}

fn show_contacts(state: &Rc<RefCell<State>>, ui: &App) {
    let search = view::Search::new(&ui.get_people_query());
    let items = with_state_value(state, |s| view::contact_items(&s.contacts, &s.selected, s.fresh, &search));
    if let Some(items) = items {
        ui.set_contacts_reveal(view::fresh_offset(ui, &items).unwrap_or(-1.0));
        ui.set_contacts(view::refill(ui.get_contacts(), items));
    }
    let count = with_state_value(state, |s| i32::try_from(s.selected.len()).unwrap_or(i32::MAX));
    ui.set_selected_count(count.unwrap_or_default());
}

/// Fills the contact page, and closes it if that key is no longer a contact. Refreshing one that
/// is not open would open it, so a change made elsewhere leaves it alone.
fn refresh_open_contact(state: &Rc<RefCell<State>>, ui: &App, peer: EndpointId) {
    let open = peer.to_string();
    if ui.get_open_contact().iter().any(|contact| contact.id == open.as_str()) {
        show_open_contact(state, ui, peer);
    }
}

fn show_open_contact(state: &Rc<RefCell<State>>, ui: &App, peer: EndpointId) {
    let found = with_contacts(state, |contacts| contacts.get(&peer).map(view::contact_detail)).flatten();
    ui.set_open_contact(maybe(found));
}

/// How many the Calls screen shows; the store keeps more than a screen can use.
const CALLS_SHOWN: i64 = 100;

/// Fills the Calls screen, newest first, grouped by day. A search looks through all that is kept.
/// Read from the database, so the list arrives a moment later; loads finish in the order they
/// were asked for (the database's lock is fair), so the last search's rows are the ones left.
fn show_calls(state: &Rc<RefCell<State>>, ui: &App) {
    let search = view::Search::new(&ui.get_calls_query());
    let limit = if search.is_empty() { CALLS_SHOWN } else { calls::KEEP };
    let Some(log) = with_state_value(state, |s| s.log.clone()) else { return };
    let (state, weak) = (Rc::clone(state), ui.as_weak());
    spawn(async move {
        let records = log.recent(limit).await.unwrap_or_else(|e| {
            tracing::warn!("reading the call log: {e}");
            Vec::new()
        });
        let Some(ui) = weak.upgrade() else { return };
        let items =
            with_state_value(&state, |s| view::call_items(&records, &s.contacts, &s.clock, &s.selected_calls, &search));
        if let Some(items) = items {
            ui.set_calls(view::refill(ui.get_calls(), items));
        }
        let count = with_state_value(&state, |s| i32::try_from(s.selected_calls.len()).unwrap_or(i32::MAX));
        ui.set_selected_calls_count(count.unwrap_or_default());
    });
}

fn with_contacts<T>(state: &Rc<RefCell<State>>, f: impl FnOnce(&Contacts) -> T) -> Option<T> {
    state.try_borrow().ok().map(|s| f(&s.contacts))
}

/// Changes the contacts through a fresh handle, so the state is never borrowed across the
/// database's await, then puts the reloaded list in the state. What changed is the store's to
/// say; the list shown is whatever it holds afterwards, failed change or not.
async fn save_contact(
    state: &Rc<RefCell<State>>,
    change: impl AsyncFnOnce(&mut Contacts) -> Result<(), uplink_core::Error>,
) -> Result<(), String> {
    let db = state.try_borrow().map_err(|_| "busy, try again".to_owned())?.contacts.db();
    let mut contacts = Contacts::open(db).await.map_err(|e| e.to_string())?;
    let changed = change(&mut contacts).await.map_err(|e| e.to_string());
    with_state(state, |s| s.contacts = contacts);
    changed
}

/// Re-reads the contacts (at launch, or after the core has written a call and stamped one) and
/// shows both lists, the calls after the contacts: a call's row is named from them.
fn reload_lists(state: &Rc<RefCell<State>>, ui: &App) {
    let (state, weak) = (Rc::clone(state), ui.as_weak());
    spawn(async move {
        if let Err(e) = save_contact(&state, async |_| Ok(())).await {
            tracing::warn!("re-reading contacts: {e}");
        }
        if let Some(ui) = weak.upgrade() {
            show_contacts(&state, &ui);
            show_calls(&state, &ui);
        }
    });
}

/// Runs `work` on the UI thread. The database's futures step their own IO, so they need no
/// runtime of their own; whatever they change in the state and the window is theirs to set.
fn spawn(work: impl Future<Output = ()> + 'static) {
    if let Err(e) = slint::spawn_local(work) {
        tracing::error!("spawning on the UI thread: {e}");
    }
}

fn with_state_value<T>(state: &Rc<RefCell<State>>, f: impl FnOnce(&mut State) -> T) -> Option<T> {
    state.try_borrow_mut().ok().map(|mut s| f(&mut s))
}

fn with_state(state: &Rc<RefCell<State>>, f: impl FnOnce(&mut State)) {
    match state.try_borrow_mut() {
        Ok(mut s) => f(&mut s),
        Err(_) => tracing::warn!("state busy; event dropped"),
    }
}

/// Asks for the camera permission (prompting if needed), then starts the camera.
fn request_camera(state: &Rc<RefCell<State>>, platform: &Rc<Platform>) {
    let (state, platform) = (Rc::clone(state), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        match platform.request_permission(Permission::Camera).await {
            Ok(true) => with_state(&state, State::start_camera),
            Ok(false) => with_state(&state, |s| s.status("camera permission denied")),
            Err(e) => {
                tracing::error!("camera permission: {e}");
                with_state(&state, |s| s.status(format!("camera permission failed: {e}")));
            }
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning camera request: {e}");
    }
}

/// The gate's three, in the order they are asked for and shown.
const GATE: [Permission; 3] = [Permission::Camera, Permission::RecordAudio, Permission::PostNotifications];

/// Reads back what the Settings rows about staying reachable say. Each is changed in a system
/// screen, so this runs whenever the app returns to the front. A failed read leaves the row as it
/// was rather than guessing.
fn refresh_reachability(ui: &App, platform: &Platform) {
    match platform.battery_unrestricted() {
        Ok(unrestricted) => ui.set_battery_unrestricted(unrestricted),
        Err(e) => tracing::warn!("reading battery optimisation: {e}"),
    }
    match platform.full_screen_calls_allowed() {
        Ok(allowed) => ui.set_full_screen_calls(allowed),
        Err(e) => tracing::warn!("reading full-screen calls: {e}"),
    }
    match platform.has_maker_list() {
        Ok(found) => ui.set_maker_list(found),
        Err(e) => tracing::warn!("looking for the maker's background list: {e}"),
    }
    tracing::info!(
        battery_unrestricted = ui.get_battery_unrestricted(),
        full_screen_calls = ui.get_full_screen_calls(),
        maker_list = ui.get_maker_list(),
        "reachability"
    );
}

/// Reads each permission's state and shows the gate while any is missing. `blocked` means Android
/// will not ask again, so the only way on is Settings.
fn refresh_gate(ui: &App, platform: &Platform) -> bool {
    // Android stops explaining both before a permission has ever been asked for and after it has
    // been refused for good, so "will not explain" only means blocked once we have asked.
    let asked = ui.get_asked();
    let mut items = Vec::with_capacity(GATE.len());
    let mut blocked = false;
    let mut all = true;
    for permission in GATE {
        let granted = platform.has_permission(permission).unwrap_or(false);
        let explains = platform.should_explain(permission).unwrap_or(false);
        let grant = if granted {
            Grant::Granted
        } else if asked && !explains {
            Grant::Blocked
        } else {
            Grant::Needed
        };
        tracing::info!(?permission, granted, explains, asked, "gate");
        all &= granted;
        blocked |= grant == Grant::Blocked;
        items.push(PermissionItem { permission: view::permission(permission), grant });
    }
    ui.set_permissions(view::list(items));
    ui.set_permissions_blocked(blocked);
    ui.set_gate(!all);
    all
}

/// Runs `task` on the UI loop, saying so if the loop is gone rather than dropping it silently.
fn spawn_ui(task: impl Future<Output = ()> + 'static) {
    if let Err(e) = slint::spawn_local(task) {
        tracing::warn!("the event loop refused a task: {e}");
    }
}

/// Recorded once the explainer has been shown, so later launches go straight to Android's own
/// dialog. A one-time grant lapses when the process dies, and re-reading the same screen every
/// launch would be a wall between the user and the prompt they already understand.
/// Answers the system back gesture with whatever is innermost on screen, and steps the app into
/// the background when there is nothing left to close. Java hands every press here rather than
/// finishing the activity, because finishing it takes the endpoint with it.
fn went_back(ui: &App) -> bool {
    if ui.get_choosing_output() {
        // Back out of the Audio sheet changes nothing.
        ui.set_choosing_output(false);
    } else if ui.get_video_asked() {
        // Back out of their ask is Keep voice: nothing turns on that was not agreed to.
        ui.set_video_asked(false);
        ui.invoke_answer_video(false);
    } else if ui.get_call_state() == CallState::Lost {
        ui.set_call_state(CallState::Idle);
    } else if ui.get_notice().row_count() > 0 {
        ui.set_notice(none());
    } else if ui.get_confirming() != Confirm::None {
        ui.set_confirming(Confirm::None);
    } else if ui.get_battery_ask() && !ui.get_gate() {
        // Back out of the explainer is "Not now", which is also what it would mean in person.
        ui.invoke_skip_battery();
    } else if ui.get_adding_relay() {
        // Back out of the sheet is Cancel: nothing is added by accident.
        ui.set_adding_relay(false);
        ui.set_new_relay_name(Default::default());
        ui.set_new_relay_url(Default::default());
    } else if ui.get_relays_open() {
        ui.set_relays_open(false);
    } else if ui.get_quality_sheet() != QualitySheet::None {
        // Back out of the picker changes nothing.
        ui.set_quality_sheet(QualitySheet::None);
    } else if ui.get_quality_open() {
        ui.set_quality_open(false);
    } else if ui.get_screenshots_open() {
        ui.set_screenshots_open(false);
    } else if ui.get_pending_key().row_count() > 0 {
        forget_pending_key(ui);
    } else if ui.get_open_contact().row_count() > 0 {
        ui.set_open_contact(none());
    } else if ui.get_open_call().row_count() > 0 {
        ui.set_open_call(none());
    } else if ui.get_scanning() {
        ui.invoke_scan(false);
    } else if ui.get_call_state() != CallState::Idle && !ui.get_call_folded() {
        // A call is never ended by going back — it folds away, as it would from its own control.
        ui.set_call_folded(true);
    } else if ui.get_selecting() {
        ui.invoke_clear_selection();
    } else if ui.get_selecting_calls() {
        ui.invoke_clear_call_selection();
    } else if ui.get_screen() == Screen::People && !ui.get_people_query().is_empty() {
        ui.set_people_query(Default::default());
        ui.invoke_search_people(Default::default());
    } else if ui.get_screen() == Screen::Calls && !ui.get_calls_query().is_empty() {
        ui.set_calls_query(Default::default());
        ui.invoke_search_calls(Default::default());
    } else if ui.get_screen() != Screen::People {
        ui.set_screen(Screen::People);
    } else {
        return false;
    }
    true
}

/// The naming sheet is done with: added, or dismissed.
fn forget_pending_key(ui: &App) {
    ui.set_pending_key(none());
    ui.set_new_name(Default::default());
    ui.set_add_problem(none());
}

/// Stored as a word rather than a number, so a row stays readable and reordering the enum cannot
/// silently change what someone chose. Anything unrecognised means following the system.
const fn appearance_name(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::System => "system",
        Appearance::Light => "light",
        Appearance::Dark => "dark",
    }
}

fn appearance_from(stored: Option<&str>) -> Appearance {
    match stored {
        Some("light") => Appearance::Light,
        Some("dark") => Appearance::Dark,
        _ => Appearance::System,
    }
}

/// Stored by its code, as the bundle names it; anything unrecognised means following the phone.
const fn language_code(language: Language) -> &'static str {
    match language {
        Language::System => "system",
        Language::English => "en",
        Language::Arabic => ARABIC,
    }
}

fn language_from(stored: Option<&str>) -> Language {
    match stored {
        Some("en") => Language::English,
        Some(ARABIC) => Language::Arabic,
        _ => Language::System,
    }
}

/// What following the phone comes to: a language uplink has words for, else English.
fn system_language() -> Language {
    let locale = sys_locale::get_locale().unwrap_or_default();
    let base = locale.split(['-', '_', '@']).next().unwrap_or_default();
    match language_from(Some(base)) {
        Language::System => Language::English,
        known => known,
    }
}

/// Switches the words to `language`, which the theme has already been told; the theme mirrors the
/// layout from the same setting.
fn show_language(ui: &App, language: Language) {
    let shown = if language == Language::System { ui.global::<Theme>().get_system_language() } else { language };
    let code = match shown {
        Language::Arabic => ARABIC,
        // English is the markup's own text: the bundle's empty name.
        Language::System | Language::English => "",
    };
    if let Err(e) = slint::select_bundled_translation(code) {
        tracing::warn!(code, "selecting the translation: {e}");
    }
}

/// Explains the battery exemption once, after the permissions the app cannot work without. Not a
/// gate: refusing it costs reachability overnight, not the app. It is only marked as offered once
/// the user chooses, so a launch killed with the page up shows it again.
fn offer_battery_exemption(ui: &App, platform: &Platform, settings: &Settings) {
    if settings.flag(settings::BATTERY_OFFERED) {
        return;
    }
    match platform.battery_unrestricted() {
        Ok(unrestricted) => ui.set_battery_ask(!unrestricted),
        Err(e) => tracing::warn!("reading battery optimisation: {e}"),
    }
}

/// The explainer was answered, either way.
fn battery_offered(ui: &App, settings: &Settings) {
    ui.set_battery_ask(false);
    let settings = settings.clone();
    spawn(async move {
        if let Err(e) = settings.set_flag(settings::BATTERY_OFFERED, true).await {
            tracing::warn!("recording the battery offer: {e}");
        }
    });
}

/// Asks for each missing permission in turn, then re-reads the gate.
fn request_gate(ui: &App, platform: &Rc<Platform>, settings: &Settings) {
    let (weak, platform, settings) = (ui.as_weak(), Rc::clone(platform), settings.clone());
    let task = slint::spawn_local(async move {
        for permission in GATE {
            match platform.request_permission(permission).await {
                Ok(granted) => tracing::info!(?permission, granted, "gate"),
                Err(e) => tracing::warn!(?permission, "gate: {e}"),
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_asked(true);
            if refresh_gate(&ui, &platform) {
                offer_battery_exemption(&ui, &platform, &settings);
            }
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning the permission gate: {e}");
    }
}

/// Asks for the microphone permission (prompting if needed), then starts the voice streams.
fn start_voice(state: &Rc<RefCell<State>>, platform: &Rc<Platform>, sender: AudioSender, receiver: AudioReceiver) {
    let (state, platform) = (Rc::clone(state), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        match platform.request_permission(Permission::RecordAudio).await {
            // Telecom has put the phone in call mode by now; the streams open into it.
            Ok(true) => with_state(&state, |s| s.start_audio(sender, receiver)),
            Ok(false) => with_state(&state, |s| s.status("microphone permission denied")),
            Err(e) => {
                tracing::error!("microphone permission: {e}");
                with_state(&state, |s| s.status(format!("microphone permission failed: {e}")));
            }
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning microphone request: {e}");
    }
}

/// Converts the newest image of `reader` into a texture, if one arrived. The caller keeps the
/// returned image until the next frame: the texture samples its buffer.
fn blit(
    reader: &ImageReader,
    preview: &mut Option<Preview>,
    turns: i32,
    mirror: bool,
) -> Result<Option<(Frame, Image)>> {
    let AcquireResult::Image(image) = reader.acquire_latest_image()? else {
        return Ok(None);
    };
    let buffer = image.hardware_buffer()?;
    if preview.is_none() {
        // SAFETY: only called from BeforeRendering, where Slint's GL context is current.
        *preview = Some(unsafe { Preview::new() }?);
    }
    let Some(preview) = preview.as_mut() else {
        return Ok(None);
    };
    let (width, height) = (u32::try_from(image.width()?)?, u32::try_from(image.height()?)?);
    // SAFETY: GL context is current; the caller keeps `image` (owner of `buffer`) alive.
    let frame = unsafe { preview.draw(buffer.as_ptr().cast(), width, height, turns, mirror) }?;
    Ok(Some((frame, image)))
}

/// Converts the latest camera image into the self-view texture, if one arrived.
fn render_local(state: &mut State, preview: &mut Option<Preview>) -> Result<Option<Frame>> {
    let Some(session) = state.session.as_mut() else {
        return Ok(None);
    };
    let turns = session.camera.upright_quarter_turns() + state.extra_turns;
    let mirror = (session.camera.facing() == Facing::Front) ^ state.mirror;
    let Some((frame, image)) = blit(&session.reader, preview, turns, mirror)? else {
        return Ok(None);
    };
    session.shown = Some(image);
    Ok(Some(frame))
}

/// Converts the latest decoded remote image into the main view's texture, if one arrived.
fn render_remote(state: &mut State, preview: &mut Option<Preview>) -> Result<Option<Frame>> {
    let Some(call) = state.call.as_mut() else {
        return Ok(None);
    };
    let Some((frame, image)) = blit(&call.remote, preview, call.remote_turns(), false)? else {
        return Ok(None);
    };
    call.shown = Some(image);
    Ok(Some(frame))
}

fn texture_image(frame: Frame) -> slint::Image {
    // SAFETY: the texture was created on this window's GL context by `Preview`.
    unsafe {
        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(frame.texture, (frame.width, frame.height).into())
    }
    .build()
}

/// Why the last run ended, into this run's log. The previous run's own log goes out with
/// [`share_diagnostics`] whole, so only what Android knows and the file cannot say is repeated.
fn log_previous_exits(platform: &Platform) {
    let exits = platform.previous_exits().unwrap_or_else(|e| format!("exit info unavailable: {e}"));
    tracing::info!("previous exits:\n{exits}");
}

/// Logs panics from any thread into this instance's log; restores the default hook on drop so a
/// later `android_main` in the same process doesn't write into a stale log.
struct PanicHook;

impl PanicHook {
    fn install(dispatch: Dispatch) -> Self {
        std::panic::set_hook(Box::new(move |info| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            tracing::dispatcher::with_default(&dispatch, || tracing::error!("panic: {info}\n{backtrace}"));
        }));
        Self
    }
}

impl Drop for PanicHook {
    fn drop(&mut self) {
        // take_hook panics when called while panicking.
        if !std::thread::panicking() {
            drop(std::panic::take_hook());
        }
    }
}

fn stats_text(state: &State, camera_frames: u32, secs: f64, cpu_percent: Option<f64>) -> String {
    let camera_fps = f64::from(camera_frames) / secs;
    let blits = state.stats.blits.swap(0, Ordering::Relaxed);
    let micros = state.stats.blit_micros.swap(0, Ordering::Relaxed);
    let blit_fps = f64::from(blits) / secs;
    let avg_micros = micros.checked_div(u64::from(blits)).unwrap_or_default();
    let cpu = cpu_percent.map_or_else(|| "n/a".to_owned(), |c| format!("{c:.0}%"));
    let remote_fps = f64::from(state.stats.remote.swap(0, Ordering::Relaxed)) / secs;
    let camera_error = state.session.as_ref().and_then(|s| s.camera.error());
    tracing::debug!(camera_fps, remote_fps, blit_fps, avg_micros, cpu, "stats");
    let mut text = format!(
        "cam {camera_fps:.1} fps · blit {blit_fps:.1} fps · {avg_micros} µs/blit · CPU {cpu} (100% = 1 core)\n\
         turns +{} · mirror {} · camera error {camera_error:?}",
        state.extra_turns, state.mirror
    );
    if let Some(call) = &state.call {
        let stats = &call.stats;
        let count = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        text.push_str(&format!(
            "\nremote {remote_fps:.1} fps · codecs {} · sent {} ({} late, {} congested) · received {} ({} dropped) · keyframe asks {}/{}",
            if call.codecs_running() { "ok" } else { "STOPPED" },
            count(&stats.frames_sent),
            count(&stats.frames_late),
            count(&stats.frames_dropped_congested),
            count(&stats.frames_received),
            count(&stats.frames_dropped_received),
            count(&stats.keyframe_requests_sent),
            count(&stats.keyframe_requests_received),
        ));
    }
    if let Some(audio) = &state.audio {
        let counts = audio.taken_counts();
        let count = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        let stats = state.call.as_ref().map(|call| &call.stats);
        let voice = stats.map_or_else(String::new, |stats| {
            format!(
                "sent {} received {} (late {}, fec {}, concealed {})",
                count(&stats.audio_sent),
                count(&stats.audio_received),
                count(&stats.audio_late),
                count(&stats.audio_fec_recovered),
                count(&stats.audio_concealed),
            )
        });
        let running = if audio.running() { "ok" } else { "STOPPED" };
        // Logged too: a silent microphone is invisible on a phone with no adb.
        tracing::info!(
            mic_samples = counts.captured,
            speaker_samples = counts.played,
            mic_lost = counts.capture_lost,
            speaker_lost = counts.playback_lost,
            running,
            "voice"
        );
        text.push_str(&format!(
            "\nvoice {running} · {voice}\nmic {} samples ({} lost) · speaker {} samples ({} lost)",
            counts.captured, counts.capture_lost, counts.played, counts.playback_lost
        ));
    }
    text
}

fn run(app: AndroidApp, data_dir: &Path) -> Result<()> {
    // Before logging, because finding out whether this process already has a core is what decides
    // whether logging needs starting at all.
    let (platform, platform_events) = Platform::attach(&app)?;
    let platform = Rc::new(platform);

    // The endpoint belongs to the process, not to this window. The Application starts it for
    // whoever asks first — this window, or the listening service after a boot — so a window
    // usually finds it bound already, along with the log writer, which must not be started a
    // second time on the same file.
    // SAFETY: the handle is the one the Application's `nativeStart` returned, held ever since.
    let Some(core) = (unsafe { Core::from_handle(platform.core_handle()?) }) else {
        anyhow::bail!("the endpoint did not start; the reason is in logcat");
    };
    // This window's threads log through the core's subscriber, whichever run created it.
    let dispatch = core.dispatch();
    let _log = tracing::dispatcher::set_default(&dispatch);
    let _panic_hook = PanicHook::install(dispatch.clone());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), filter = LOG_FILTER, sdk = platform.sdk(), "starting");
    log_previous_exits(&platform);
    let avc = platform.avc()?;
    // Only once the endpoint is really up, so the notification never claims a readiness the app
    // does not have.
    if let Err(e) = platform.set_listening(true) {
        tracing::warn!("staying reachable: {e}");
    }
    let identity = *core.id();
    // The core's stores, which it opened: the settings are its one shared copy. The contacts
    // start empty so the window can draw at once, and fill as soon as the database answers.
    let contacts = Contacts::empty(core.db());
    let log = core.log();
    let settings = core.settings();
    let runtime = core.runtime().handle().clone();

    let state = Rc::new(RefCell::new(State {
        ui: slint::Weak::default(),
        session: None,
        audio: None,
        call: None,
        avc,
        runtime: runtime.clone(),
        facing: Facing::Front,
        extra_turns: 0,
        mirror: false,
        resume_camera: false,
        camera_watch: CameraWatch::default(),
        scanning: false,
        scan_busy: Arc::default(),
        contacts,
        log,
        selected: FxHashSet::default(),
        selected_calls: FxHashSet::default(),
        connected_at: None,
        mode: Mode::Video,
        parked: None,
        peer: None,
        camera_on: true,
        reconnecting: false,
        telecom: TelecomState::default(),
        chrome: None,
        presets: None,
        sending: None,
        rate: None,
        media: None,
        reach: CoreReach::Connecting,
        network_moved: None,
        health: Health::default(),
        me: identity,
        fresh: None,
        clock: LocalClock::new(platform.context()),
        stats: Arc::default(),
        core: None,
        settings: settings.clone(),
        capture: Capture::saved(settings.flag(settings::BLOCK_CAPTURE), settings.flag(settings::ASK_BLOCK_CAPTURE)),
        relays: None,
    }));

    let lifecycle = Rc::clone(&state);
    let resumed = Rc::downgrade(&platform);
    let (windowed, window_platform) = (app.clone(), Rc::downgrade(&platform));
    slint::android::init_with_event_listener(app, move |event| match event {
        // A new surface each time: it votes again for the screen's fastest rate.
        PollEvent::Main(MainEvent::InitWindow { .. }) => {
            if let (Some(window), Some(platform)) = (windowed.native_window(), window_platform.upgrade()) {
                vote_frame_rate(&window, &platform);
            }
        }
        // A call keeps the camera: the foreground service is what allows that in the background.
        PollEvent::Main(MainEvent::Pause) => with_state(&lifecycle, |s| {
            s.resume_camera = s.call.is_none() && s.session.take().is_some();
        }),
        PollEvent::Main(MainEvent::Resume { .. }) => with_state(&lifecycle, |s| {
            if std::mem::take(&mut s.resume_camera) {
                s.start_camera();
            }
            // Coming back from Settings is the common case: re-read rather than asking the user
            // to confirm they did what they just did.
            if let (Some(ui), Some(platform)) = (s.ui.upgrade(), resumed.upgrade()) {
                refresh_gate(&ui, &platform);
                refresh_reachability(&ui, &platform);
            }
        }),
        _ => {}
    })?;

    let ui = App::new()?;
    state.borrow_mut().ui = ui.as_weak();
    ui.set_my_fingerprint(view::fingerprint_lines(&identity));
    match qr_image(&identity) {
        Ok((image, mark)) => {
            ui.set_qr(image);
            ui.set_qr_mark(mark);
        }
        Err(e) => tracing::error!("identity qr: {e:#}"),
    }
    reload_lists(&state, &ui);

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_search_people(move |_| {
        if let Some(ui) = weak.upgrade() {
            show_contacts(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_search_calls(move |_| {
        if let Some(ui) = weak.upgrade() {
            show_calls(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_selected(move |id| {
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        let empty = with_state_value(&s, |state| {
            if !state.selected.remove(&peer) {
                state.selected.insert(peer);
            }
            state.selected.is_empty()
        });
        if let Some(ui) = weak.upgrade() {
            // Unticking the last one ends selection, as it does in any Android list; there is
            // no Done to press, and nothing selected has nothing to do.
            if empty == Some(true) {
                ui.set_selecting(false);
            }
            show_contacts(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_clear_selection(move || {
        with_state(&s, |state| state.selected.clear());
        if let Some(ui) = weak.upgrade() {
            ui.set_selecting(false);
            show_contacts(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_selected(move || {
        // Taken first so a failure part-way leaves the rest of the list alone.
        let chosen = with_state_value(&s, |state| state.selected.drain().collect::<Vec<_>>()).unwrap_or_default();
        if let Some(ui) = weak.upgrade() {
            ui.set_selecting(false);
        }
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let mut failed = 0usize;
            for peer in &chosen {
                if save_contact(&s, async |contacts| contacts.remove_id(*peer).await.map(drop)).await.is_err() {
                    failed += 1;
                }
            }
            let Some(ui) = weak.upgrade() else { return };
            show_contacts(&s, &ui);
            if failed > 0 {
                let (count, total) = (i32::try_from(failed), i32::try_from(chosen.len()));
                let (count, total) = (count.unwrap_or(i32::MAX), total.unwrap_or(i32::MAX));
                view::say_counted(&ui, Toast { say: Say::SomeNotRemoved, subject: Default::default(), count, total });
            }
        });
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_open_contact_page(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        show_open_contact(&s, &ui, peer);
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_set_favourite(move |id, favourite| {
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let outcome = save_contact(&s, async |contacts| contacts.set_favourite(peer, favourite).await).await;
            let Some(ui) = weak.upgrade() else { return };
            match outcome {
                Ok(()) => {
                    show_contacts(&s, &ui);
                    refresh_open_contact(&s, &ui, peer);
                }
                Err(e) => toast(&ui, Say::Failed, e),
            }
        });
    });

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_clear_calls(move || {
        let Some(log) = with_state_value(&s, |state| state.log.clone()) else { return };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            if let Err(e) = log.clear().await {
                tracing::warn!("clearing the call log: {e}");
            }
            if let Some(ui) = weak.upgrade() {
                show_calls(&s, &ui);
            }
        });
    });
    // A call's details, read fresh from the log rather than from the list's model, which only
    // carries what a row shows.
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_show_call(move |entry| {
        let Ok(id) = entry.parse::<CallId>() else { return };
        let Some(log) = with_state_value(&s, |state| state.log.clone()) else { return };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let found = log.get(id).await;
            let Some(ui) = weak.upgrade() else { return };
            match found {
                Ok(Some(logged)) => {
                    let detail = with_state_value(&s, |state| {
                        let saved = state.contacts.name_of(&logged.call.peer).map(str::to_owned);
                        view::call_detail(&logged, saved, &state.clock)
                    });
                    if let Some(detail) = detail {
                        ui.set_open_call(one(detail));
                    }
                }
                // Removed or trimmed since the list was drawn: the list is what is stale.
                Ok(None) => show_calls(&s, &ui),
                Err(e) => toast(&ui, Say::CallUnreadable, e),
            }
        });
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_call(move |entry| {
        let Ok(id) = entry.parse::<CallId>() else { return };
        let Some(log) = with_state_value(&s, |state| state.log.clone()) else { return };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let removed = log.remove(&[id]).await;
            let Some(ui) = weak.upgrade() else { return };
            show_calls(&s, &ui);
            if let Err(e) = removed {
                toast(&ui, Say::CallNotRemoved, e);
            }
        });
    });
    // Selecting calls works as selecting contacts does, on its own set of ticks.
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_call_selected(move |entry| {
        let Ok(id) = entry.parse::<CallId>() else { return };
        let empty = with_state_value(&s, |state| {
            if !state.selected_calls.remove(&id) {
                state.selected_calls.insert(id);
            }
            state.selected_calls.is_empty()
        });
        if let Some(ui) = weak.upgrade() {
            if empty == Some(true) {
                ui.set_selecting_calls(false);
            }
            show_calls(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_clear_call_selection(move || {
        with_state(&s, |state| state.selected_calls.clear());
        if let Some(ui) = weak.upgrade() {
            ui.set_selecting_calls(false);
            show_calls(&s, &ui);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_selected_calls(move || {
        let Some((log, chosen)) =
            with_state_value(&s, |state| (state.log.clone(), state.selected_calls.drain().collect::<Vec<_>>()))
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            ui.set_selecting_calls(false);
        }
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let removed = log.remove(&chosen).await;
            let Some(ui) = weak.upgrade() else { return };
            show_calls(&s, &ui);
            if let Err(e) = removed {
                toast(&ui, Say::CallsNotRemoved, e);
            }
        });
    });

    // Android's dialogs only ever follow a tap on the gate, never a launch: a dialog over a screen
    // nobody has read yet is asking cold. With the gate already open, the battery offer is the one
    // thing left to ask at startup.
    if refresh_gate(&ui, &platform) {
        offer_battery_exemption(&ui, &platform, &settings);
    }

    let (weak, p, s) = (ui.as_weak(), Rc::clone(&platform), settings.clone());
    ui.on_grant_permissions(move || {
        if let Some(ui) = weak.upgrade() {
            request_gate(&ui, &p, &s);
        }
    });
    let (weak, p) = (ui.as_weak(), Rc::clone(&platform));
    ui.on_open_settings(move || {
        if let Err(e) = p.open_app_settings()
            && let Some(ui) = weak.upgrade()
        {
            toast(&ui, Say::SettingUnopened, e);
        }
    });

    refresh_reachability(&ui, &platform);
    let (weak, p, s) = (ui.as_weak(), Rc::clone(&platform), settings.clone());
    ui.on_allow_battery(move || {
        // From the explainer or from Settings alike. The explainer stays up until Android's
        // dialog has closed, so the user is never moved on before they have answered.
        let (weak, p, s) = (weak.clone(), Rc::clone(&p), s.clone());
        spawn_ui(async move {
            let answered = p.request_battery_exemption().await;
            let Some(ui) = weak.upgrade() else { return };
            match answered {
                Ok(unrestricted) => {
                    tracing::info!(unrestricted, "battery exemption answered");
                    ui.set_battery_unrestricted(unrestricted);
                    if ui.get_battery_ask() {
                        battery_offered(&ui, &s);
                    }
                }
                Err(e) => toast(&ui, Say::BatterySettings, e),
            }
        });
    });
    let (weak, s) = (ui.as_weak(), settings.clone());
    ui.on_skip_battery(move || {
        if let Some(ui) = weak.upgrade() {
            tracing::info!("battery exemption declined for now");
            battery_offered(&ui, &s);
        }
    });
    let (weak, p) = (ui.as_weak(), Rc::clone(&platform));
    ui.on_allow_full_screen_calls(move || {
        if let Err(e) = p.open_full_screen_calls_settings()
            && let Some(ui) = weak.upgrade()
        {
            toast(&ui, Say::SettingUnopened, e);
        }
    });
    let (weak, p) = (ui.as_weak(), Rc::clone(&platform));
    ui.on_open_maker_list(move || {
        if let Err(e) = p.open_maker_list()
            && let Some(ui) = weak.upgrade()
        {
            toast(&ui, Say::ListUnopened, e);
        }
    });

    // What the user chose last time, before anything can report a change back.
    let theme = ui.global::<Theme>();
    // The user's own touch-and-hold delay; the markup's default stands if it cannot be read.
    match platform.long_press_timeout().map(|t| i64::try_from(t.as_millis())) {
        Ok(Ok(millis)) => theme.set_long_press(millis),
        Ok(Err(e)) => tracing::warn!("long-press timeout out of range: {e}"),
        Err(e) => tracing::warn!("reading the long-press timeout: {e}"),
    }
    let p = Rc::clone(&platform);
    ui.on_long_pressed(move || {
        if let Err(e) = p.long_press_feedback() {
            tracing::debug!("long-press feedback: {e}");
        }
    });
    theme.set_appearance(appearance_from(settings.get(settings::APPEARANCE).as_deref()));
    // The phone's language is read once, at launch: Android restarts the activity when it changes.
    theme.set_system_language(system_language());
    let language = language_from(settings.get(settings::LANGUAGE).as_deref());
    theme.set_language(language);
    show_language(&ui, language);

    let s = settings.clone();
    ui.on_appearance_changed(move |appearance| {
        let s = s.clone();
        spawn(async move {
            if let Err(e) = s.set(settings::APPEARANCE, appearance_name(appearance)).await {
                tracing::warn!("storing the appearance: {e}");
            }
        });
    });
    let (s, weak) = (settings.clone(), ui.as_weak());
    let p = Rc::clone(&platform);
    ui.on_language_changed(move |language| {
        if let Some(ui) = weak.upgrade() {
            show_language(&ui, language);
        }
        let code = language_code(language);
        let stored = s.clone();
        spawn(async move {
            if let Err(e) = stored.set(settings::LANGUAGE, code).await {
                tracing::warn!("storing the language: {e}");
            }
        });
        // Notifications speak it too. Restarting the listening service re-posts the one that is
        // always up; the rest pick it up the next time they are posted.
        if let Err(e) = p.context().set_language(core::locale(Some(code))) {
            tracing::warn!("telling Java the language: {e}");
        }
        if let Err(e) = p.set_listening(true) {
            tracing::warn!("re-posting the listening notification: {e}");
        }
    });
    // Every relay change is written, shown and handed to the pilot at once: it changes the map on
    // the live endpoint, so there is no Save and no rebind.
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_set_relays_auto(move |auto| {
        let settings = s.borrow().settings.clone();
        let mut choice = Choice::load(&settings);
        choice.auto = auto;
        save_relays(&s, &weak, "the relay mode", async move || choice.save(&settings).await);
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_tick_relay(move |url, on| {
        let Ok(url) = url.parse::<RelayUrl>() else {
            tracing::warn!(%url, "ticked a relay that is not a URL");
            return;
        };
        let settings = s.borrow().settings.clone();
        let mut choice = Choice::load(&settings);
        choice.ticked.retain(|ticked| *ticked != url);
        if on {
            choice.ticked.push(url);
        }
        // The last tick stays: with none, the phone could not be called at all.
        if choice.ticked.is_empty() {
            return;
        }
        save_relays(&s, &weak, "the relay ticks", async move || choice.save(&settings).await);
    });
    let s = Rc::clone(&state);
    ui.on_check_relays(move || steer_relays(&s, Steer::Check));
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_add_relay(move |name, url| {
        let settings = s.borrow().settings.clone();
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let added = relays::add_custom(&settings, &name, &url).await;
            let Some(ui) = weak.upgrade() else { return };
            match added {
                Ok(()) => {
                    ui.set_adding_relay(false);
                    ui.set_new_relay_name(Default::default());
                    ui.set_new_relay_url(Default::default());
                    show_relays(&s, &ui);
                    steer_relays(&s, Steer::Reload);
                }
                Err(uplink_core::Error::RelayUrl(url)) => toast(&ui, Say::RelayInvalid, url),
                Err(e) => toast(&ui, Say::Failed, e),
            }
        });
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_relay(move |url| {
        let Ok(parsed) = url.parse::<RelayUrl>() else {
            tracing::warn!(%url, "removing a relay that is not a URL");
            return;
        };
        let settings = s.borrow().settings.clone();
        save_relays(&s, &weak, "the relay removal", async move || relays::remove_custom(&settings, &parsed).await);
    });
    show_relays(&state, &ui);

    // The window's own `init` has already run by the time callbacks are set, so the first state
    // is sent from here; the callback only carries the changes after it.
    let p = Rc::clone(&platform);
    ui.on_bars_changed(move |light| {
        if let Err(e) = p.set_light_system_bars(light) {
            tracing::warn!("system bar appearance: {e}");
        }
    });
    if let Err(e) = platform.set_light_system_bars(ui.get_bars_light()) {
        tracing::warn!("system bar appearance: {e}");
    }

    // Back is accepted in the markup, which is what stops Slint's own handler from finishing the
    // activity; this decides what it meant.
    let (weak, p) = (ui.as_weak(), Rc::clone(&platform));
    ui.on_back(move || {
        let Some(ui) = weak.upgrade() else { return };
        if !went_back(&ui)
            && let Err(e) = p.move_to_background()
        {
            tracing::warn!("stepping into the background: {e}");
        }
    });

    // What the platform did unasked: the window shrinking into picture-in-picture, and the taps
    // on the buttons that window carries. The receiver needs no reactor, so the UI loop owns it.
    let (weak, mut events, s, p) = (ui.as_weak(), platform_events, Rc::clone(&state), Rc::clone(&platform));
    spawn_ui(async move {
        while let Some(event) = events.recv().await {
            let Some(ui) = weak.upgrade() else { break };
            match event {
                PlatformEvent::PictureInPicture(active) => ui.set_call_pip(active),
                PlatformEvent::Hangup => ui.invoke_hangup(),
                PlatformEvent::ToggleMic => ui.invoke_toggle_mic(),
                PlatformEvent::Answer => ui.invoke_accept(),
                // The only times on screen are the call log's; the contacts' are relative.
                PlatformEvent::ClockChanged => {
                    with_state(&s, |state| state.clock.forget());
                    show_calls(&s, &ui);
                }
                PlatformEvent::CallAudio => {
                    follow_telecom(&s, &ui, &p);
                    sync_chrome(&s, &ui, &p);
                }
                PlatformEvent::NextOutput => next_output(&s, &ui, &p),
            }
        }
    });

    // The endpoint is already bound by the time the window exists, so the splash only has to last
    // as long as the first frame.
    state.borrow_mut().core = Some(Arc::clone(&core));
    ui.set_booting(false);
    // While this window is up it answers for the app; handing the stream back on the way out is
    // what lets anything else take over. Attached now rather than inside the task: Answer from
    // the notification can be waiting already, and the call it connects has to find this window
    // attached, since that is where its media goes.
    let events = core.attach();
    let (s, weak, p, c) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform), Arc::clone(&core));
    spawn_ui(async move {
        handle_node_events(events, weak, s, p).await;
        c.detach();
    });

    let (s, weak, p) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform));
    ui.on_call(move |key, voice| match peer_key(&key, &identity) {
        Ok(peer) => place_call(&s, &weak, &p, peer, if voice { Mode::Voice } else { Mode::Video }),
        Err(unusable) => {
            if let Some(ui) = weak.upgrade() {
                toast(&ui, unusable.say(), "");
            }
        }
    });
    // The same person, the same way, from the screen that said the last call was lost.
    let (s, weak, p) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform));
    ui.on_call_again(move || {
        let again = with_state_value(&s, |state| state.peer.map(|peer| (peer, state.mode))).flatten();
        if let Some(ui) = weak.upgrade() {
            ui.set_call_state(CallState::Idle);
        }
        if let Some((peer, mode)) = again {
            place_call(&s, &weak, &p, peer, mode);
        }
    });
    let weak = ui.as_weak();
    ui.on_close_call(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_call_state(CallState::Idle);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_camera(move || {
        let on = with_state_value(&s, State::toggle_camera);
        if let (Some(on), Some(ui)) = (on, weak.upgrade()) {
            ui.set_camera_on(on);
        }
        tell_media(&s, &weak);
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_ask_video(move |ask| {
        if send_call_command(&s, Command::AskVideo(ask), &weak)
            && let Some(ui) = weak.upgrade()
        {
            ui.set_video_asking(ask);
            // Asking again is the answer they gave last time no longer standing.
            ui.set_kept_voice(false);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_answer_video(move |accept| {
        send_call_command(&s, Command::AnswerVideo(accept), &weak);
    });
    let (s, weak, p) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform));
    ui.on_accept(move || {
        if send_call_command(&s, Command::Answer(true), &weak)
            && let Some(ui) = weak.upgrade()
        {
            let mode = with_state_value(&s, |state| state.mode).unwrap_or(Mode::Video);
            start_call_service(&p, &ui.get_peer_name(), mode);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_reject(move || {
        send_call_command(&s, Command::Answer(false), &weak);
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_hangup(move || {
        send_call_command(&s, Command::Hangup, &weak);
    });

    let s = Rc::clone(&state);
    ui.on_flip_camera(move || {
        with_state(&s, |s| {
            s.facing = s.facing.flipped();
            if s.session.is_some() {
                s.start_camera();
            }
        });
    });
    let (s, weak, p) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform));
    ui.on_toggle_mic(move || {
        let muted = with_state_value(&s, State::toggle_mic).unwrap_or_default();
        if let Some(ui) = weak.upgrade() {
            ui.set_mic_on(!muted);
        }
        // A muted mic sounds exactly like a quiet room: the other side is told which it is.
        tell_media(&s, &weak);
        // The picture-in-picture window carries its own mute button; it has to agree.
        if let Err(e) = p.set_mic_on(!muted) {
            tracing::warn!("mic state for the call window: {e}");
        }
    });
    // Always the sheet, even with only the earpiece and the speaker: which one is in use should
    // be something you can see, and mute sits there too.
    let weak = ui.as_weak();
    ui.on_audio(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_choosing_output(true);
        }
    });
    // Any output, even the one that was in use under Mute, brings their voice back.
    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_choose_output(move |index| {
        silence(&s, &weak, false);
        if let (Ok(index), Some(ui)) = (usize::try_from(index), weak.upgrade()) {
            choose_output(&s, &ui, &p, index);
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_silence(move || silence(&s, &weak, true));

    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_open_quality(move || {
        if let Some(ui) = weak.upgrade() {
            show_quality(&s, &ui, &p);
        }
    });
    // Chosen from the page; a call in progress changes over at once.
    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_choose_quality(move |wifi, preset| {
        let network = if wifi { PresetNetwork::Wifi } else { PresetNetwork::Mobile };
        let preset = view::core_preset(preset);
        tracing::info!(?network, ?preset, "call quality chosen");
        let Some(settings) = with_state_value(&s, |state| state.settings.clone()) else { return };
        let (s, p, weak) = (Rc::clone(&s), Rc::clone(&p), weak.clone());
        // Shown and put in force once saved: both read the choice back from the settings.
        spawn(async move {
            if let Err(e) = preset.choose(&settings, network).await {
                tracing::warn!(?network, ?preset, "saving the call quality: {e}");
            }
            if let Some(ui) = weak.upgrade() {
                show_quality(&s, &ui, &p);
            }
            requalify(&s, &p);
        });
    });
    show_quality(&state, &ui, &platform);

    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_choose_screenshots(move |choice| {
        if let Some(ui) = weak.upgrade() {
            choose_capture(&s, &p, &ui, choice);
        }
    });
    show_capture(&state, &ui);
    apply_capture(&state, &platform);

    // Read by the endpoint at each incoming call, so saving it is all it takes.
    ui.set_reject_unknown(settings.flag(settings::REJECT_UNKNOWN));
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_reject_unknown(move || {
        let Some(ui) = weak.upgrade() else { return };
        let on = !ui.get_reject_unknown();
        tracing::info!(on, "reject unknown callers");
        let Some(settings) = with_state_value(&s, |state| state.settings.clone()) else { return };
        let weak = weak.clone();
        spawn(async move {
            match settings.set_flag(settings::REJECT_UNKNOWN, on).await {
                Ok(()) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_reject_unknown(on);
                    }
                }
                Err(e) => tracing::warn!("saving reject unknown callers: {e}"),
            }
        });
    });

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_add_contact(move |name, key| {
        let Some(ui) = weak.upgrade() else { return };
        let name = name.trim().to_owned();
        let problem = |error, subject: &str| AddProblem { error, subject: subject.into() };
        let checked = peer_key(&key, &identity).map_err(|unusable| problem(unusable.add_error(), "")).and_then(|id| {
            // The same key and the same name are different mistakes, and the store's one
            // error for both cannot say which: a new name would fix only the second.
            let taken = with_contacts(&s, |contacts| {
                let saved = contacts.name_of(&id).map(str::to_owned);
                (saved, contacts.iter().any(|contact| contact.name == name))
            });
            match taken {
                Some((Some(saved), _)) => Err(problem(AddError::AlreadySaved, &saved)),
                Some((None, true)) => Err(problem(AddError::NameTaken, &name)),
                _ => Ok(id),
            }
        });
        // Under the field, in the sheet that is still open: the user can fix it right there.
        let show_problem = |ui: &App, problem: AddProblem| {
            tracing::info!(error = ?problem.error, subject = %problem.subject, "adding a contact");
            ui.set_add_problem(one(problem));
        };
        let id = match checked {
            Ok(id) => id,
            Err(problem) => return show_problem(&ui, problem),
        };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let saved = save_contact(&s, async |contacts| contacts.add(&name, id).await).await;
            let Some(ui) = weak.upgrade() else { return };
            match saved {
                Ok(()) => {
                    forget_pending_key(&ui);
                    // Added from a call's details, perhaps: that page has done its job.
                    ui.set_open_call(none());
                    show_added(&s, &ui, id);
                }
                Err(e) => show_problem(&ui, problem(AddError::Failed, &e)),
            }
        });
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_rename_contact(move |key, name| {
        let Ok(id) = EndpointId::from_str(key.trim()) else { return };
        let name = name.trim().to_owned();
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let outcome = save_contact(&s, async |contacts| contacts.rename(id, &name).await).await;
            match (outcome, weak.upgrade()) {
                (Ok(()), Some(ui)) => {
                    show_contacts(&s, &ui);
                    // The sheet is still open on this contact, so it re-reads too — otherwise it
                    // keeps showing the old name until it is closed and opened again.
                    refresh_open_contact(&s, &ui, id);
                }
                (Err(e), Some(ui)) => toast(&ui, Say::Failed, e),
                _ => {}
            }
        });
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_contact(move |key| {
        let Ok(id) = EndpointId::from_str(key.trim()) else { return };
        let (s, weak) = (Rc::clone(&s), weak.clone());
        spawn(async move {
            let outcome = save_contact(&s, async |contacts| contacts.remove_id(id).await.map(drop)).await;
            match (outcome, weak.upgrade()) {
                (Ok(()), Some(ui)) => show_contacts(&s, &ui),
                (Err(e), Some(ui)) => toast(&ui, Say::Failed, e),
                _ => {}
            }
        });
    });
    let (s, p) = (Rc::clone(&state), Rc::clone(&platform));
    ui.on_pick_key(move || pick_key(&s, &p));
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_key_found(move |key| {
        let Some(ui) = weak.upgrade() else { return };
        // Parsed on the scanning thread already; this only fails if the markup ever sends junk.
        match EndpointId::from_str(&key) {
            Ok(key) => offer_key(&s, &ui, key),
            Err(e) => tracing::warn!("a found key that does not parse: {e}"),
        }
    });
    let (p, weak, dir) = (Rc::clone(&platform), ui.as_weak(), data_dir.to_path_buf());
    let handle = runtime.clone();
    ui.on_share_key(move || {
        // The picture, not the key: a code is what someone points a camera at, and it is what
        // arrives if they save it and open it from the other side. Drawing and writing it are
        // the runtime's work, not this tap's. The caption is read here: Java is the UI thread's.
        let caption = match p.context().text(Text::CardCaption) {
            Ok(caption) => caption,
            Err(e) => {
                tracing::error!("reading the card's caption: {e}");
                if let Some(ui) = weak.upgrade() {
                    toast(&ui, Say::CodeNotShared, "");
                }
                return;
            }
        };
        let card = write_identity_card(dir.clone(), identity.to_string(), caption);
        share_when_written(&handle, &p, &weak, SHARE_FILE, Text::ShareIdentity, Say::CodeNotShared, card);
    });
    let (p, weak) = (Rc::clone(&platform), ui.as_weak());
    ui.on_copy_key(move || {
        // What Android calls the copied key in its own confirmation.
        let copied = p.context().text(Text::CopyKeyLabel).and_then(|label| p.copy_text(&label, &identity.to_string()));
        if let Err(e) = copied
            && let Some(ui) = weak.upgrade()
        {
            toast(&ui, Say::KeyNotCopied, e);
        }
    });
    let (p, weak, dir, handle) = (Rc::clone(&platform), ui.as_weak(), data_dir.to_path_buf(), runtime.clone());
    ui.on_share_diagnostics(move || {
        // The set can be a hundred megabytes before it compresses; packing it is the runtime's.
        let packing = pack_diagnostics(dir.clone());
        share_when_written(&handle, &p, &weak, DIAGNOSTICS_FILE, Text::ShareDiagnostics, Say::LogNotPacked, packing);
    });
    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_scan(move |on| {
        with_state(&s, |s| s.scanning = on);
        if let Some(ui) = weak.upgrade() {
            ui.set_scanning(on);
        }
        if on {
            request_camera(&s, &p);
            return;
        }
        // A call keeps its camera, rebuilt without the scan stream. Outside one, the camera goes:
        // it used to be rebuilt anyway, and a front camera ran behind the Connect page for as
        // long as it was open, feeding a picture nothing showed.
        let in_call = weak.upgrade().is_some_and(|ui| in_call(&ui));
        with_state(&s, |s| {
            if in_call && s.session.is_some() {
                s.start_camera();
            } else {
                s.session = None;
                s.clear_frame();
            }
        });
    });

    let s = Rc::clone(&state);
    let (mut local, mut remote): (Option<Preview>, Option<Preview>) = (None, None);
    ui.window().set_rendering_notifier(move |rendering, _| match rendering {
        RenderingState::BeforeRendering => with_state(&s, |state| {
            let started = Instant::now();
            match render_local(state, &mut local) {
                Ok(Some(frame)) => {
                    state.stats.blits.fetch_add(1, Ordering::Relaxed);
                    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    state.stats.blit_micros.fetch_add(micros, Ordering::Relaxed);
                    if let Some(ui) = state.ui.upgrade() {
                        ui.set_frame(texture_image(frame));
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("preview: {e:#}"),
            }
            match render_remote(state, &mut remote) {
                Ok(Some(frame)) => {
                    if let Some(ui) = state.ui.upgrade() {
                        ui.set_remote_frame(texture_image(frame));
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("remote view: {e:#}"),
            }
        }),
        RenderingState::RenderingTeardown => {
            for preview in [local.take(), remote.take()].into_iter().flatten() {
                // SAFETY: Slint keeps the GL context current during RenderingTeardown.
                unsafe { preview.destroy() };
            }
            // The images name textures that are gone now. Kept, they draw whatever the new
            // context reuses the number for — the app's own mark, over a call whose video had
            // stopped — until a new frame replaces them.
            with_state(&s, |state| {
                if let Some(ui) = state.ui.upgrade() {
                    ui.set_frame(slint::Image::default());
                    ui.set_remote_frame(slint::Image::default());
                }
            });
        }
        _ => {}
    })?;

    let stats_timer = Timer::default();
    let (s, p, window) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    let mut last = (Instant::now(), cpu::process_seconds());
    let mut ticks: u32 = 0;
    stats_timer.start(TimerMode::Repeated, STATS_INTERVAL, move || {
        let now = (Instant::now(), cpu::process_seconds());
        let secs = now.0.duration_since(last.0).as_secs_f64();
        let cpu_percent = last.1.zip(now.1).map(|(before, after)| (after - before) / secs * PERCENT);
        last = now;
        ticks += 1;
        let pip = window.upgrade().is_some_and(|ui| ui.get_call_pip());
        let camera_paused = with_state_value(&s, |state| {
            state.recover_audio();
            let camera_frames = state.stats.camera.swap(0, Ordering::Relaxed);
            let camera_paused = state.watch_camera(pip);
            // A codec that dies mid-call takes the picture with it and says nothing otherwise.
            if state.call.as_ref().is_some_and(|call| !call.codecs_running()) {
                state.video_failed();
            }
            // Counted every second, written every few: the log is read by whoever is fixing a
            // call that has already happened, and a line a second would bury it.
            let text = stats_text(state, camera_frames, secs, cpu_percent);
            // Only while there is something to measure: idle, it was a line every five seconds
            // saying nothing, all day, into a log that rolls by size.
            let busy = state.call.is_some() || state.session.is_some();
            if busy && ticks.is_multiple_of(STATS_LOG_EVERY) {
                tracing::info!("{text}");
            }
            // From the call's own counters, which a voice call has too.
            let route = state.media.as_ref().map_or(Route::Unknown, |media| media.route());
            let stalled = state.media.as_ref().is_some_and(|media| media.stalled());
            // A call that has stopped is the overlay's to say; the pill is for one that still works.
            let weak = match &state.media {
                Some(media) if !stalled && !state.reconnecting => state.health.sample(media),
                _ => Weak::None,
            };
            // Only while there is a path to judge; what piles up while there is none is not it.
            if stalled || state.reconnecting {
                if let Some(rate) = &mut state.rate {
                    rate.pause();
                }
            } else {
                state.pace();
            }
            let moved = state.network_moved.is_some_and(|at| at.elapsed() < UNSETTLED);
            if let Some(ui) = state.ui.upgrade() {
                // Only while it runs: a lost call's screen keeps the length it ended at.
                if state.connected_at.is_some() {
                    ui.set_call_timer(state.call_timer().into());
                }
                ui.set_call_route(view::route(route));
                ui.set_call_reconnecting(state.reconnecting || stalled);
                ui.set_call_stall(view::stall(state.reach, moved));
                ui.set_call_weak(view::weak(weak));
                ui.set_call_capture(state.capture.shown(state.connected_at.map(|at| at.elapsed())));
            }
            camera_paused
        });
        if camera_paused == Some(true) {
            tell_media(&s, &window);
        }
        // A call starting, ending, turning video or going on hold: within the second.
        if let Some(ui) = window.upgrade() {
            sync_chrome(&s, &ui, &p);
        }
    });

    let outcome = ui.run();
    if let Err(e) = platform.set_call_service(false, "", false) {
        tracing::warn!("stopping call service: {e}");
    }
    // Only what this window owned. The endpoint and the runtime stay: they belong to the process,
    // and tearing them down here is exactly what used to make a swiped-away app unreachable.
    {
        let mut state = state.borrow_mut();
        state.session = None;
        state.audio = None;
        state.call = None;
        state.core = None;
    }
    tracing::info!("window closed, endpoint still bound");
    Ok(outcome?)
}

/// Why a code cannot be someone to call.
#[derive(Clone, Copy, Debug)]
enum Unusable {
    /// Some other app's QR, or text that is not a key at all.
    NotAKey,
    /// This phone's own identity. Calling yourself is not a feature, and a contact that is you
    /// would ring nothing.
    Yours,
}

impl Unusable {
    /// Said on its own, as a toast.
    const fn say(self) -> Say {
        match self {
            Self::NotAKey => Say::NotAKey,
            Self::Yours => Say::OwnCode,
        }
    }

    /// Said under the name field, in the sheet that asked.
    const fn add_error(self) -> AddError {
        match self {
            Self::NotAKey => AddError::NotAKey,
            Self::Yours => AddError::OwnCode,
        }
    }
}

/// A scanned, opened or pasted code only counts if it is one of our keys and not this phone's
/// own; saying which beats silently filling the field with it.
fn peer_key(text: &str, me: &EndpointId) -> Result<EndpointId, Unusable> {
    let key = EndpointId::from_str(text.trim()).map_err(|_| Unusable::NotAKey)?;
    if key == *me {
        return Err(Unusable::Yours);
    }
    Ok(key)
}

/// Starts the call's foreground service on the tap that places or answers it. Camera and
/// microphone service types are only granted while the app is in front, and by the time a call
/// connects the user may have gone elsewhere — which Android answered with a SecurityException
/// that took the whole app down. Starting here also arms picture-in-picture while it rings.
/// Whether a call is up or on its way. A lost call's screen is only saying so.
fn in_call(ui: &App) -> bool {
    !matches!(ui.get_call_state(), CallState::Idle | CallState::Lost)
}

/// Dials `peer`, voice or video. One call at a time, said here rather than dialled and refused:
/// the node refusing is silent, and the optimistic Dialing below would have renamed the call
/// that is up.
fn place_call(state: &Rc<RefCell<State>>, weak: &slint::Weak<App>, platform: &Platform, peer: EndpointId, mode: Mode) {
    let Some(ui) = weak.upgrade() else { return };
    if in_call(&ui) {
        toast(&ui, Say::AlreadyInCall, "");
        return;
    }
    if send_call_command(state, Command::Call(peer, mode), weak) {
        // Optimistic: the node confirms with Dialing, or reverts via Ended.
        ui.set_call_state(CallState::Dialing);
        ui.set_call_voice(mode == Mode::Voice);
        let name =
            with_contacts(state, |contacts| view::name_of(contacts, &peer)).unwrap_or_else(|| view::short(&peer));
        set_peer(&ui, &name);
        start_call_service(platform, &name, mode);
    }
}

/// What a call needs outside the window's own pixels: the screen, and the small window's output
/// button.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Chrome {
    screen: CallScreen,
    /// `None` is Mute.
    output: Option<RouteKind>,
}

impl Chrome {
    /// As Signal does it: video keeps the screen on, a voice call on the earpiece turns it off at
    /// the ear, and anything else (speaker, a headset, on hold, no call) leaves it alone.
    fn of(ui: &App, telecom: &TelecomState) -> Self {
        let silenced = ui.get_call_silenced();
        let routed = telecom.current.and_then(|index| telecom.routes.get(index)).map(|route| route.kind);
        let live = matches!(ui.get_call_state(), CallState::Dialing | CallState::Ringing | CallState::Connected);
        let screen = if !live || ui.get_call_held() {
            CallScreen::Normal
        } else if !ui.get_call_voice() {
            CallScreen::On
        } else if routed == Some(RouteKind::Phone) && !silenced {
            CallScreen::Proximity
        } else {
            CallScreen::Normal
        };
        let output = if silenced { None } else { Some(routed.unwrap_or(RouteKind::Speaker)) };
        Self { screen, output }
    }
}

/// Tells Android what the call needs of the screen and the small window, when that changed.
fn sync_chrome(state: &Rc<RefCell<State>>, ui: &App, platform: &Platform) {
    let Some(telecom) = with_state_value(state, |s| s.telecom.clone()) else { return };
    let now = Chrome::of(ui, &telecom);
    let Some(before) = with_state_value(state, |s| s.chrome.replace(now)) else { return };
    if before.map(|c| c.screen) != Some(now.screen)
        && let Err(e) = platform.set_call_screen(now.screen)
    {
        tracing::warn!(screen = ?now.screen, "call screen: {e}");
    }
    if before.map(|c| c.output) != Some(now.output)
        && let Err(e) = platform.set_pip_output(now.output)
    {
        tracing::warn!("small window's output button: {e}");
    }
}

/// The small window's output button: the next output in Telecom's order, then Mute, then round
/// again, with no sheet in between.
fn next_output(state: &Rc<RefCell<State>>, ui: &App, platform: &Platform) {
    let Some(telecom) = with_state_value(state, |s| s.telecom.clone()) else { return };
    let choices = telecom.routes.len() + 1;
    let at = if ui.get_call_silenced() { telecom.routes.len() } else { telecom.current.unwrap_or_default() };
    let next = (at + 1) % choices;
    if next == telecom.routes.len() {
        silence(state, &ui.as_weak(), true);
        sync_chrome(state, ui, platform);
    } else {
        silence(state, &ui.as_weak(), false);
        choose_output(state, ui, platform, next);
    }
}

/// Mute as an output: their voice stops playing, here only. Our mic and what they see of us
/// stay as they are.
fn silence(state: &Rc<RefCell<State>>, ui: &slint::Weak<App>, silenced: bool) {
    with_state(state, |s| {
        if let Some(audio) = &s.audio {
            audio.set_silenced(silenced);
        }
    });
    if let Some(ui) = ui.upgrade()
        && ui.get_call_silenced() != silenced
    {
        tracing::info!(silenced, "their voice");
        ui.set_call_silenced(silenced);
    }
}

/// Asks Telecom for one of the outputs it listed, and shows it as chosen at once: until Telecom
/// reports it, what it reports is the output being left, and the Audio key and the small window's
/// button would flash that first. Its report confirms this, or puts back what it did instead.
fn choose_output(state: &Rc<RefCell<State>>, ui: &App, platform: &Platform, index: usize) {
    with_state(state, |s| {
        s.telecom.current = Some(index);
        // Rerouting restarts our voice streams; the gap is ours, not their network's.
        s.health.ours_restarted();
    });
    if let Ok(shown) = i32::try_from(index) {
        ui.set_call_output(shown);
    }
    if let Err(e) = platform.context().telecom_choose_route(index) {
        tracing::warn!(index, "choosing an output: {e}");
    }
    sync_chrome(state, ui, platform);
}

/// Telecom changed the call's hold, mute or outputs: read what it says now and follow it.
fn follow_telecom(state: &Rc<RefCell<State>>, ui: &App, platform: &Platform) {
    let now = match platform.context().telecom_state() {
        Ok(now) => now,
        Err(e) => {
            tracing::warn!("reading the call from telecom: {e}");
            return;
        }
    };
    let before = with_state_value(state, |s| std::mem::replace(&mut s.telecom, now.clone())).unwrap_or_default();
    ui.set_call_outputs(view::outputs(&now.routes));
    ui.set_call_output(now.current.and_then(|index| i32::try_from(index).ok()).unwrap_or(NO_OUTPUT));
    ui.set_call_held(now.held);
    if now.routes != before.routes || now.current != before.current {
        tracing::info!(routes = ?now.routes, current = ?now.current, "call outputs");
        // Android reroutes, and our voice streams restart with it.
        with_state(state, |s| s.health.ours_restarted());
    }
    if now.held != before.held {
        tracing::info!(held = now.held, "call hold");
        with_state(state, |s| s.apply_hold(now.held));
        if now.held {
            ui.set_choosing_output(false);
        }
        tell_media(state, &ui.as_weak());
    }
    // A headset's mute button, or the system's own: the call's mute follows it, and says so.
    if now.muted != before.muted && now.muted == ui.get_mic_on() {
        ui.invoke_toggle_mic();
    }
}

/// Asks for the screen's fastest refresh rate for our surface. Android leaves an app at 60 unless
/// it asks, and weighs a surface's vote only while it is drawing: 120 while scrolling or
/// animating, and the panel free to slow down again when the app sits still.
fn vote_frame_rate(window: &NativeWindow, platform: &Platform) {
    let peak = match platform.peak_refresh_rate() {
        Ok(peak) if peak > 0.0 => peak,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!("reading the screen's refresh rates: {e}");
            return;
        }
    };
    match window.set_frame_rate(peak, FrameRateCompatibility::Default) {
        Ok(()) => tracing::info!(fps = peak, "surface frame rate"),
        Err(e) => tracing::warn!(fps = peak, "surface frame rate: {e}"),
    }
}

/// Writes a relay change, then shows it and has the pilot reload, in that order: the pilot reads
/// the settings the write lands in, so reloading first would use the old ones.
fn save_relays(
    state: &Rc<RefCell<State>>,
    weak: &slint::Weak<App>,
    what: &'static str,
    save: impl AsyncFnOnce() -> Result<(), uplink_core::Error> + 'static,
) {
    let (state, weak) = (Rc::clone(state), weak.clone());
    spawn(async move {
        if let Err(e) = save().await {
            tracing::warn!("storing {what}: {e}");
        }
        if let Some(ui) = weak.upgrade() {
            show_relays(&state, &ui);
        }
        steer_relays(&state, Steer::Reload);
    });
}

/// Tells Java whether our windows may be captured: the user's choice, or the call's ask.
fn apply_capture(state: &Rc<RefCell<State>>, platform: &Platform) {
    let Some(secure) = with_state_value(state, |s| s.capture.secure()) else { return };
    if let Err(e) = platform.context().set_secure(secure) {
        tracing::warn!(secure, "screen capture: {e}");
    }
}

/// The screenshots choice, as the store has it.
fn show_capture(state: &Rc<RefCell<State>>, ui: &App) {
    if let Some(choice) = with_state_value(state, |s| s.capture.choice()) {
        ui.set_screenshots(choice);
    }
}

/// Saves the screenshots choice as its two flags and puts it in force at once, mid-call too.
fn choose_capture(state: &Rc<RefCell<State>>, platform: &Platform, ui: &App, choice: Screenshots) {
    let chosen = with_state_value(state, |s| {
        s.capture.choose(choice);
        tracing::info!(?choice, "screenshots choice");
        (s.settings.clone(), s.capture.block, s.capture.ask)
    });
    // In force at once from the state; saved behind it.
    if let Some((settings, block, ask)) = chosen {
        spawn(async move {
            let saved = match settings.set_flag(settings::BLOCK_CAPTURE, block).await {
                Ok(()) => settings.set_flag(settings::ASK_BLOCK_CAPTURE, ask).await,
                failed => failed,
            };
            if let Err(e) = saved {
                tracing::warn!("saving the screenshots choice: {e}");
            }
        });
    }
    apply_capture(state, platform);
    show_capture(state, ui);
    tell_media(state, &ui.as_weak());
}

/// Tells the other side what our mic and camera are doing now.
fn tell_media(state: &Rc<RefCell<State>>, weak: &slint::Weak<App>) {
    if let Some(media) = with_state_value(state, |s| s.media_state()) {
        send_call_command(state, Command::Media(media), weak);
    }
}

/// A voice call holds the microphone type alone: asking for a camera it never uses is wrong, and
/// one more thing Android can refuse.
fn start_call_service(platform: &Platform, peer: &str, mode: Mode) {
    if let Err(e) = platform.set_call_service(true, peer, mode == Mode::Video) {
        tracing::warn!("call service: {e}");
    }
}

/// Returns whether the node accepted the command. The endpoint may not exist yet, since it binds
/// while the window is already showing.
fn send_call_command(state: &Rc<RefCell<State>>, command: Command, ui: &slint::Weak<App>) -> bool {
    let Some(core) = state.borrow().core.clone() else {
        tracing::warn!(?command, "the endpoint is still starting");
        return false;
    };
    // Asked for here, not cached: a rebind between two calls replaces it.
    match core.calls().try_send(command) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(?command, "node command: {e}");
            if let Some(ui) = ui.upgrade() {
                toast(&ui, Say::Failed, e);
            }
            false
        }
    }
}

/// The peer an event concerns, when it names one.
const fn peer_of(event: &Event) -> Option<EndpointId> {
    match event {
        Event::Dialing { peer, .. }
        | Event::Ringing { peer }
        | Event::Incoming { peer, .. }
        | Event::Connected { peer, .. } => Some(*peer),
        _ => None,
    }
}

/// The quality steps this phone's front camera and encoder can send, asked once per window: the
/// answers do not change while it runs, and each takes a trip through the camera service.
fn sendable(state: &Rc<RefCell<State>>, platform: &Platform) -> Vec<Preset> {
    if let Some(known) = with_state_value(state, |s| s.presets.clone()).flatten() {
        return known;
    }
    let context = platform.context();
    let steps: Vec<Preset> = Preset::ALL
        .into_iter()
        .filter(|preset| {
            context.can_send(preset.video()).unwrap_or_else(|e| {
                tracing::warn!(?preset, "asking whether this phone can send it: {e}");
                false
            })
        })
        .collect();
    tracing::info!(?steps, "quality steps this phone can send");
    with_state(state, |s| s.presets = Some(steps.clone()));
    steps
}

/// The network the phone is on, as the presets divide them.
fn preset_network(platform: &Platform) -> PresetNetwork {
    match platform.context().on_wifi() {
        Ok(true) => PresetNetwork::Wifi,
        Ok(false) => PresetNetwork::Mobile,
        Err(e) => {
            tracing::warn!("asking which network this is: {e}");
            PresetNetwork::Mobile
        }
    }
}

/// The step a call sends at now: the one chosen for this network, or the nearest below it this
/// phone can send.
fn preset_now(state: &Rc<RefCell<State>>, platform: &Platform) -> Preset {
    let network = preset_network(platform);
    let steps = sendable(state, platform);
    let chosen = with_state_value(state, |s| Preset::chosen(&s.settings, network)).unwrap_or(network.default_preset());
    chosen.within(&steps)
}

/// Sets the call's step, and tells the core, which writes it into the call's log.
fn send_at(state: &Rc<RefCell<State>>, preset: Preset) {
    with_state(state, |s| {
        s.sending = Some(preset);
        if let Some(core) = &s.core {
            core.set_sending(Some(preset.video()));
        }
    });
}

/// The network changed, or the choice did, mid-call: a different step changes over at once. The
/// voice from its next frame; the picture through a new encoder under the same sender, the
/// camera pointed at it, and a keyframe by nature.
fn requalify(state: &Rc<RefCell<State>>, platform: &Platform) {
    let Some(before) = with_state_value(state, |s| s.sending).flatten() else { return };
    let now = preset_now(state, platform);
    if now == before {
        return;
    }
    tracing::info!(?before, ?now, "call quality changes over");
    send_at(state, now);
    with_state(state, |s| {
        if let Some(audio) = &s.audio {
            audio.set_voice_bps(now.voice_bps());
        }
        let swapped = match &mut s.call {
            Some(call) => match call.reconfigure(&s.avc, video_config(now)) {
                Ok(()) => Some(Arc::clone(&call.stats)),
                Err(e) => {
                    tracing::warn!("changing the call's video over: {e:#}");
                    None
                }
            },
            None => None,
        };
        // A new network is a new path: what the old one could carry says nothing about it.
        if let Some(stats) = &swapped {
            s.pace_from(now, stats);
        }
        let swapped = swapped.is_some();
        if swapped && s.session.is_some() {
            s.start_camera();
        }
    });
}

/// The Call quality page and Settings' row: each network's step (what this phone can send of
/// what was chosen), the steps it can send at all, and which network it is on.
fn show_quality(state: &Rc<RefCell<State>>, ui: &App, platform: &Platform) {
    let steps = sendable(state, platform);
    let effective = |network: PresetNetwork| {
        let chosen =
            with_state_value(state, |s| Preset::chosen(&s.settings, network)).unwrap_or(network.default_preset());
        view::preset_item(chosen.within(&steps))
    };
    ui.set_quality_wifi(effective(PresetNetwork::Wifi));
    ui.set_quality_mobile(effective(PresetNetwork::Mobile));
    ui.set_quality_steps(ModelRc::new(VecModel::from(steps.into_iter().map(view::preset_item).collect::<Vec<_>>())));
    ui.set_on_wifi(preset_network(platform) == PresetNetwork::Wifi);
}

/// Starts the codecs, then the camera with the encoder as a second output.
fn start_call_video(state: &Rc<RefCell<State>>, platform: &Rc<Platform>, parts: VideoParts) {
    with_state(state, |s| s.start_video(parts));
    request_camera(state, platform);
}

/// UI call state implied by an event; `None` leaves it unchanged.
const fn call_state(event: &Event) -> Option<CallState> {
    match event {
        Event::Ready { .. }
        | Event::Reach(_)
        | Event::Network(_)
        | Event::PeerMedia(_)
        | Event::VideoAsked(_)
        | Event::VideoOn
        | Event::VideoDeclined
        | Event::Reconnecting
        | Event::Reconnected
        | Event::Relays(_) => None,
        Event::Dialing { .. } => Some(CallState::Dialing),
        Event::Ringing { .. } => Some(CallState::Ringing),
        Event::Incoming { .. } => Some(CallState::Incoming),
        Event::Connected { .. } => Some(CallState::Connected),
        Event::Ended { .. } => Some(CallState::Idle),
    }
}

fn describe(event: &Event) -> String {
    match event {
        Event::Ready { id } => format!("ready as {}", id.fmt_short()),
        Event::Reach(reach) => format!("reach: {reach:?}"),
        Event::Network(up) => format!("network {}", if *up { "up" } else { "gone" }),
        Event::Dialing { peer, mode } => format!("dialing {} ({mode:?})", peer.fmt_short()),
        Event::Ringing { peer } => format!("ringing {}", peer.fmt_short()),
        Event::Incoming { peer, mode } => format!("incoming {mode:?} call from {}", peer.fmt_short()),
        Event::Connected { peer, key_exchange, mode, .. } => {
            format!("connected to {} [{key_exchange:?}] ({mode:?})", peer.fmt_short())
        }
        Event::PeerMedia(state) => format!("their mic off {}, camera off {}", state.mic_off, state.camera_off),
        Event::VideoAsked(asking) => format!("they ask to switch to video: {asking}"),
        Event::VideoOn => "switched to video".to_owned(),
        Event::VideoDeclined => "they kept it voice".to_owned(),
        Event::Reconnecting => "connection lost; reconnecting".to_owned(),
        Event::Reconnected => "reconnected".to_owned(),
        Event::Ended { peer, reason } => match peer {
            Some(peer) => format!("call with {} ended: {reason:?}", peer.fmt_short()),
            None => format!("call ended: {reason:?}"),
        },
        Event::Relays(view) => format!(
            "relays: home {}, active {}",
            view.home.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string),
            view.active.len()
        ),
    }
}

/// Runs on the UI thread (tokio channels work on any executor); applies node events to the UI
/// and starts or stops call video. Ends when the window does, which is when the core goes back to
/// answering for itself.
async fn handle_node_events(
    mut events: mpsc::Receiver<Event>,
    ui: slint::Weak<App>,
    state: Rc<RefCell<State>>,
    platform: Rc<Platform>,
) {
    while let Some(event) = events.recv().await {
        tracing::info!("{}", describe(&event));
        let Some(ui) = ui.upgrade() else { break };
        if let Some(call_state) = call_state(&event) {
            ui.set_call_state(call_state);
        }
        // Name whoever is on the other end, by nickname when we know them.
        if let Some(peer) = peer_of(&event) {
            let name = with_contacts(&state, |contacts| view::name_of(contacts, &peer));
            set_peer(&ui, &name.unwrap_or_else(|| view::short(&peer)));
        }
        match &event {
            // Whatever went wrong last time was about last time.
            Event::Dialing { mode, peer } | Event::Incoming { mode, peer } => {
                ui.set_video_failed(false);
                ui.set_call_voice(*mode == Mode::Voice);
                ui.set_camera_on(true);
                ui.set_peer_mic_off(false);
                ui.set_peer_camera_off(false);
                ui.set_video_asking(false);
                ui.set_video_asked(false);
                ui.set_kept_voice(false);
                ui.set_call_reconnecting(false);
                ui.set_peer_held(false);
                ui.set_call_held(false);
                ui.set_choosing_output(false);
                ui.set_call_silenced(false);
                ui.set_call_outputs(ModelRc::default());
                ui.set_call_output(NO_OUTPUT);
                let (mode, peer) = (*mode, *peer);
                with_state(&state, |s| {
                    s.mode = mode;
                    s.peer = Some(peer);
                    s.camera_on = true;
                    s.reconnecting = false;
                    s.telecom = TelecomState::default();
                    (s.capture.peer_asked, s.capture.peer_blocked) = (false, false);
                });
                ui.set_call_capture(CallCapture::None);
                apply_capture(&state, &platform);
                // The ask goes with the call from the start: the core tells it on connecting.
                tell_media(&state, &ui.as_weak());
            }
            Event::PeerMedia(theirs) => {
                ui.set_peer_mic_off(theirs.mic_off);
                ui.set_peer_camera_off(theirs.camera_off);
                ui.set_peer_held(theirs.held);
                let asked_now = with_state_value(&state, |s| {
                    let before = s.capture.peer_asked;
                    (s.capture.peer_asked, s.capture.peer_blocked) = (theirs.capture_asked, theirs.capture_blocked);
                    before != theirs.capture_asked
                });
                if asked_now == Some(true) {
                    tracing::info!(asked = theirs.capture_asked, "they ask for no screen capture");
                    apply_capture(&state, &platform);
                    // Our screen's answer: blocked now, or not any more.
                    tell_media(&state, &ui.as_weak());
                }
                if let Some(shown) =
                    with_state_value(&state, |s| s.capture.shown(s.connected_at.map(|at| at.elapsed())))
                {
                    ui.set_call_capture(shown);
                }
            }
            Event::VideoAsked(asking) => ui.set_video_asked(*asking),
            Event::VideoDeclined => {
                ui.set_video_asking(false);
                ui.set_kept_voice(true);
            }
            Event::Reconnecting | Event::Reconnected => {
                let reconnecting = matches!(event, Event::Reconnecting);
                with_state(&state, |s| s.reconnecting = reconnecting);
                ui.set_call_reconnecting(reconnecting);
            }
            Event::Relays(live) => {
                with_state(&state, |s| s.relays = Some(live.clone()));
                show_relays(&state, &ui);
            }
            // The core has already written the call and stamped the contact; read both back.
            Event::Ended { peer, reason } => {
                reload_lists(&state, &ui);
                // In front, the notice is on screen; the core's notification is for when it is not.
                if let (Some(peer), EndReason::Incompatible { behind, theirs }) = (peer, reason) {
                    let name = with_contacts(&state, |contacts| view::name_of(contacts, peer));
                    let ours = state.borrow().core.as_ref().map(|core| core.app().to_owned());
                    // The wire and the core say an unknown version with an empty one.
                    let version = |version: Option<&str>| maybe(version.filter(|v| !v.is_empty()).map(Into::into));
                    ui.set_notice(one(Mismatch {
                        ours_behind: *behind == Behind::Us,
                        name: name.unwrap_or_else(|| view::short(peer)).into(),
                        theirs: version(Some(theirs)),
                        ours: version(ours.as_deref()),
                    }));
                }
            }
            _ => {}
        }
        match event {
            Event::Reach(reach) => {
                with_state(&state, |s| s.reach = reach);
                ui.set_reach(view::reach(reach));
            }
            Event::Network(_) => {
                with_state(&state, |s| s.network_moved = Some(Instant::now()));
                // Which network is "now" on the page, and which step a call sends at.
                show_quality(&state, &ui, &platform);
                requalify(&state, &platform);
            }
            Event::Connected { media, mode, .. } => {
                with_state(&state, |s| {
                    s.connected_at = Some(Instant::now());
                    s.mode = mode;
                    s.health = Health::default();
                });
                let preset = preset_now(&state, &platform);
                tracing::info!(?preset, network = ?preset_network(&platform), "call quality");
                send_at(&state, preset);
                let MediaSession { video, incoming_video, keyframe_requests, audio, incoming_audio, stats } = *media;
                with_state(&state, |s| s.media = Some(Arc::clone(&stats)));
                let parts = VideoParts {
                    sender: video,
                    incoming: incoming_video,
                    keyframe_requests,
                    stats: Arc::clone(&stats),
                };
                // The peer is already named on the window; the notification names them too.
                start_call_service(&platform, &ui.get_peer_name(), mode);
                match mode {
                    Mode::Video => start_call_video(&state, &platform, parts),
                    // No camera and no codecs: the battery and data a voice call should cost.
                    Mode::Voice => with_state(&state, |s| s.parked = Some(parts)),
                }
                start_voice(&state, &platform, audio, incoming_audio);
            }
            // The screen changes over in place: same call, same timer.
            Event::VideoOn => {
                ui.set_call_voice(false);
                ui.set_video_asking(false);
                ui.set_video_asked(false);
                ui.set_kept_voice(false);
                let parked = with_state_value(&state, |s| {
                    s.mode = Mode::Video;
                    s.parked.take()
                });
                if let Some(parts) = parked.flatten() {
                    start_call_service(&platform, &ui.get_peer_name(), Mode::Video);
                    start_call_video(&state, &platform, parts);
                }
            }
            Event::Ended { reason, .. } => {
                let answered = with_state_value(&state, |s| {
                    let answered = s.connected_at.is_some();
                    s.connected_at = None;
                    s.sending = None;
                    s.media = None;
                    s.reconnecting = false;
                    s.end_call();
                    (s.capture.peer_asked, s.capture.peer_blocked) = (false, false);
                    answered
                });
                ui.set_call_capture(CallCapture::None);
                apply_capture(&state, &platform);
                // A call the network ended stays on screen a moment to say so, as a call that
                // never connected would; Close or Call again, or it goes by itself.
                if answered == Some(true) && matches!(reason, EndReason::ConnectionLost) {
                    ui.set_call_state(CallState::Lost);
                    let weak = ui.as_weak();
                    Timer::single_shot(LOST_LINGER, move || {
                        if let Some(ui) = weak.upgrade()
                            && ui.get_call_state() == CallState::Lost
                        {
                            ui.set_call_state(CallState::Idle);
                        }
                    });
                }
                if let Err(e) = platform.set_call_service(false, "", false) {
                    tracing::warn!("stopping call service: {e}");
                }
            }
            _ => {}
        }
    }
}

/// May run several times per process (Android reuses processes), so nothing here is global: the
/// panic hook is scoped to this call, and logging is the core's, which outlives it.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from(FALLBACK_DATA_DIR));
    // Straight to logcat: this is reached when the subscriber never started, or after it has
    // gone with the core, so it is the one path that cannot rely on `tracing`.
    if let Err(e) = run(app, &data_dir) {
        log::logcat(LOG_TAG, Level::ERROR, &format!("fatal: {e:#}"));
    }
}
