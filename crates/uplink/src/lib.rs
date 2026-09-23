//! uplink Android entry point: calls with camera video through the zero-copy GL path, with
//! in-app diagnostics (previous exits + previous log) since there is no adb.

mod audio;
mod tasks;
mod ui;
mod video;

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
use ndk::native_window::NativeWindow;
use slint::android::AndroidApp;
use slint::android::android_activity::{MainEvent, PollEvent};
use slint::{ComponentHandle, RenderingState, Timer, TimerMode};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{Dispatch, Level};
use uplink_android::camera::{Camera, Facing};
use uplink_android::codec::{Avc, VideoConfig};
use uplink_android::platform::{Permission, Platform};
use uplink_android::preview::{Frame, Preview};
use uplink_android::{cpu, log};
use uplink_core::audio::{AudioReceiver, AudioSender};
use uplink_core::contacts::Contacts;
use uplink_core::qr;
use uplink_core::media::MediaSession;
use uplink_core::node::{Command, Event, Network, Node, NodeHandle};
use uplink_core::{EndpointId, identity};

use crate::audio::CallAudio;
use crate::ui::{App, CallState, ContactItem, Screen};
use crate::video::{CallVideo, VideoParts};

const LOG_TAG: &CStr = c"uplink";
/// Baked in at build time (`just log=debug apk`).
const LOG_FILTER: &str = match option_env!("UPLINK_LOG") {
    Some(filter) => filter,
    None => "info",
};
const FALLBACK_DATA_DIR: &str = "/data/local/tmp";
const PREVIOUS_LOG_TAIL_LINES: usize = 20;
/// Lines kept in the in-app log view.
const LOG_VIEW_LINES: usize = 300;

const CAPTURE_WIDTH: i32 = 1280;
const CAPTURE_HEIGHT: i32 = 720;
const CAPTURE_FPS: i32 = 30;
const READER_MAX_IMAGES: i32 = 4;
/// Groups of four, the way the key is read aloud.
const FINGERPRINT_GROUP: usize = 4;
const FINGERPRINT_GROUPS: usize = 8;
/// A contact row and the "Your key" row show only the leading groups.
const FINGERPRINT_ROW_GROUPS: usize = 4;
const FINGERPRINT_SELF_GROUPS: usize = 3;
const QR_PIXELS: u32 = 512;
// Scanning: CPU-readable frames, big enough to read a code held up to the camera.
const SCAN_WIDTH: i32 = 960;
const SCAN_HEIGHT: i32 = 720;
const SCAN_MAX_IMAGES: i32 = 2;
const VIDEO_BITRATE: i32 = 2_000_000;
const KEYFRAME_INTERVAL_SECS: i32 = 2;
const VIDEO: VideoConfig = VideoConfig {
    width: CAPTURE_WIDTH,
    height: CAPTURE_HEIGHT,
    fps: CAPTURE_FPS,
    bitrate: VIDEO_BITRATE,
    keyframe_interval_secs: KEYFRAME_INTERVAL_SECS,
};
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const PERCENT: f64 = 100.0;
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(2);

#[derive(Default)]
struct FrameStats {
    camera: AtomicU32,
    remote: AtomicU32,
    blits: AtomicU32,
    blit_micros: AtomicU64,
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
    scanning: bool,
    /// One decode at a time; frames that arrive meanwhile are dropped.
    scan_busy: Arc<AtomicBool>,
    contacts: Contacts,
    connected_at: Option<Instant>,
    stats: Arc<FrameStats>,
}

impl State {
    fn status(&self, message: impl AsRef<str>) {
        if let Some(ui) = self.ui.upgrade() {
            append_log(&ui, message.as_ref());
        }
    }

    /// Assumes the camera permission is granted (see [`request_camera`]).
    fn start_camera(&mut self) {
        self.session = None;
        match self.open_session() {
            Ok(session) => {
                let camera = &session.camera;
                tracing::info!(id = camera.id(), facing = ?camera.facing(), orientation = camera.sensor_orientation(), "camera started");
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
        let camera = Camera::open(facing, &windows, CAPTURE_FPS)?;
        Ok(Session { shown: None, camera, reader, _scanner: scanner })
    }

    /// A small CPU-readable stream: QR needs the pixels, which the preview's GPU-only buffers
    /// never expose. Only the luma plane is read — that is already the greyscale a decoder wants.
    fn open_scanner(&self) -> Result<ImageReader> {
        let mut reader = ImageReader::new(SCAN_WIDTH, SCAN_HEIGHT, ImageFormat::YUV_420_888, SCAN_MAX_IMAGES)?;
        let (ui, runtime, busy) = (self.ui.clone(), self.runtime.clone(), Arc::clone(&self.scan_busy));
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
                        let Some(key) = found else { return };
                        tracing::info!("scanned a key");
                        // `invoke_scan` runs the same handler the button does, so the camera
                        // rebuilds without this reader.
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            ui.set_peer_key(key.into());
                            ui.set_screen(Screen::People);
                            ui.invoke_scan(false);
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
        match CallVideo::start(parts, &self.avc, VIDEO, self.runtime.clone(), on_remote_frame) {
            Ok(call) => self.call = Some(call),
            Err(e) => {
                tracing::error!("call video: {e:#}");
                self.status(format!("call video failed: {e:#}"));
            }
        }
    }

    /// Returns whether the microphone ended up muted.
    fn toggle_mic(&mut self) -> bool {
        self.audio.as_ref().is_some_and(CallAudio::toggle_mute)
    }

    /// mm:ss since the call connected.
    fn call_timer(&self) -> String {
        let elapsed = self.connected_at.map(|at| at.elapsed().as_secs()).unwrap_or_default();
        const SECONDS_PER_MINUTE: u64 = 60;
        format!("{:02}:{:02}", elapsed / SECONDS_PER_MINUTE, elapsed % SECONDS_PER_MINUTE)
    }

    /// Starts the microphone and speaker; the call keeps running without them.
    fn start_audio(&mut self, sender: AudioSender, receiver: AudioReceiver) {
        self.audio = Some(CallAudio::start(sender, receiver, self.runtime.clone()));
    }

    /// Reopens voice streams that AAudio disconnected (re-routing, headphones). Runs on the UI
    /// thread's timer, which is what AAudio requires: never from a stream callback.
    fn recover_audio(&mut self) {
        let Some(audio) = &mut self.audio else { return };
        if !audio.needs_reopen() {
            return;
        }
        match audio.reopen() {
            Ok(()) => self.status("voice streams reopened"),
            Err(e) => tracing::warn!("reopening voice streams: {e:#}"),
        }
    }

    /// Stops the call's codecs and streams; a running camera restarts without the encoder.
    fn end_call(&mut self) {
        if self.call.is_none() && self.audio.is_none() {
            return;
        }
        let camera_was_running = self.session.take().is_some();
        self.audio = None;
        self.call = None;
        if camera_was_running {
            self.start_camera();
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

/// Asks for an image and reads the key out of it, for a code that arrived over chat.
fn pick_key(state: &Rc<RefCell<State>>, platform: &Rc<Platform>) {
    let (state, platform) = (Rc::clone(state), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        let picked = platform.pick_image().await;
        let Some(ui) = state.try_borrow().ok().and_then(|s| s.ui.upgrade()) else { return };
        match picked {
            Ok(Some(image)) => match qr::decode_luma(&image.pixels, image.width, image.height, image.width) {
                Some(key) => {
                    ui.set_peer_key(key.into());
                    ui.set_screen(Screen::People);
                    ui.set_call_status("Key read — give them a name".into());
                }
                None => ui.set_call_status("No code in that image".into()),
            },
            Ok(None) => {}
            Err(e) => {
                tracing::error!("picking an image: {e}");
                ui.set_call_status(format!("could not open that image: {e}").into());
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
    let initial = name.chars().next().unwrap_or('?').to_uppercase().to_string();
    ui.set_peer_initial(initial.into());
}

/// Key as groups of four over two even lines, matching what the peer reads out. Lines rather than
/// one wrapping string, because Slint has no line-height and the design's leading matters.
fn fingerprint_lines(id: &EndpointId) -> slint::ModelRc<slint::SharedString> {
    let key = id.to_string();
    let lines: Vec<slint::SharedString> = key
        .chars()
        .take(FINGERPRINT_GROUP * FINGERPRINT_GROUPS)
        .collect::<Vec<_>>()
        .chunks(FINGERPRINT_GROUP * FINGERPRINT_ROW_GROUPS)
        .map(|line| {
            line.chunks(FINGERPRINT_GROUP).map(|group| group.iter().collect::<String>()).collect::<Vec<_>>().join(" ").into()
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(lines))
}

/// The same key abbreviated for a list row, where only enough to tell two contacts apart fits.
fn short_fingerprint(id: &EndpointId) -> String {
    groups(id, FINGERPRINT_ROW_GROUPS, " · ")
}

fn groups(id: &EndpointId, count: usize, separator: &str) -> String {
    id.to_string()
        .chars()
        .take(FINGERPRINT_GROUP * count)
        .collect::<Vec<_>>()
        .chunks(FINGERPRINT_GROUP)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(separator)
}

fn short(id: &EndpointId) -> String {
    id.fmt_short().to_string()
}

/// The key as a QR image. `qrcode` draws it; we only widen its greyscale to the RGB Slint takes.
fn qr_image(id: &EndpointId) -> Result<slint::Image> {
    let (luma, side) = qr::render(&id.to_string(), QR_PIXELS)?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    for (pixel, value) in buffer.make_mut_slice().iter_mut().zip(luma) {
        *pixel = slint::Rgb8Pixel { r: value, g: value, b: value };
    }
    Ok(slint::Image::from_rgb8(buffer))
}

/// Contacts for the list, in the order they were added.
fn show_contacts(state: &Rc<RefCell<State>>, ui: &App) {
    let items = with_contacts(state, |contacts| {
        contacts
            .iter()
            .map(|contact| ContactItem {
                name: contact.name.clone().into(),
                id: contact.id.to_string().into(),
                fingerprint: short_fingerprint(&contact.id).into(),
                initial: contact.name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                tint: 0,
            })
            .collect::<Vec<_>>()
    });
    if let Some(items) = items {
        ui.set_contacts(slint::ModelRc::new(slint::VecModel::from(items)));
    }
}

fn with_contacts<T>(state: &Rc<RefCell<State>>, f: impl FnOnce(&Contacts) -> T) -> Option<T> {
    state.try_borrow().ok().map(|s| f(&s.contacts))
}

/// Applies a change and writes contacts.toml; the error is what the UI shows.
fn save_contact(
    state: &Rc<RefCell<State>>,
    change: impl FnOnce(&mut Contacts) -> Result<(), uplink_core::Error>,
) -> Result<(), String> {
    let mut state = state.try_borrow_mut().map_err(|_| "busy, try again".to_owned())?;
    change(&mut state.contacts).map_err(|e| e.to_string())?;
    let runtime = state.runtime.clone();
    runtime.block_on(state.contacts.save()).map_err(|e| e.to_string())
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

/// Asks once for permission to show the call notification; the service runs without it.
fn request_notifications(platform: &Rc<Platform>) {
    let platform = Rc::clone(platform);
    let task = slint::spawn_local(async move {
        match platform.request_permission(Permission::PostNotifications).await {
            Ok(granted) => tracing::info!(granted, "notification permission"),
            Err(e) => tracing::warn!("notification permission: {e}"),
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning notification request: {e}");
    }
}

/// Asks for the microphone permission (prompting if needed), then puts the device in call audio
/// mode and starts the voice streams.
fn start_voice(
    state: &Rc<RefCell<State>>,
    platform: &Rc<Platform>,
    sender: AudioSender,
    receiver: AudioReceiver,
) {
    let (state, platform) = (Rc::clone(state), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        match platform.request_permission(Permission::RecordAudio).await {
            Ok(true) => {
                if let Err(e) = platform.set_in_call(true) {
                    tracing::warn!("call audio mode: {e}");
                }
                with_state(&state, |s| s.start_audio(sender, receiver));
            }
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

fn previous_run_report(platform: &Platform, data_dir: &Path) -> String {
    let exits = platform.previous_exits().unwrap_or_else(|e| format!("exit info unavailable: {e}"));
    let previous_log = std::fs::read_to_string(data_dir.join(log::PREVIOUS_LOG_FILE)).unwrap_or_default();
    let lines: Vec<&str> = previous_log.lines().collect();
    let tail = lines[lines.len().saturating_sub(PREVIOUS_LOG_TAIL_LINES)..].join("\n");
    format!("previous exits:\n{exits}\nprevious log tail:\n{tail}")
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

fn stats_text(state: &State, secs: f64, cpu_percent: Option<f64>) -> String {
    let camera_fps = f64::from(state.stats.camera.swap(0, Ordering::Relaxed)) / secs;
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

fn run(app: AndroidApp, data_dir: &Path, dispatch: Dispatch) -> Result<()> {
    tracing::info!(version = env!("CARGO_PKG_VERSION"), filter = LOG_FILTER, "starting");
    let platform = Rc::new(Platform::attach(&app)?);
    let report = previous_run_report(&platform, data_dir);
    let avc = platform.avc()?;

    let runtime = uplink_core::runtime::build(dispatch)?;
    let secret = runtime.block_on(identity::load_or_create(data_dir))?;
    let identity = secret.public();
    let contacts = runtime.block_on(Contacts::load(data_dir))?;
    let (node, node_events) = runtime.block_on(Node::start(secret, Network::N0))?;
    let calls = node.handle();

    let state = Rc::new(RefCell::new(State {
        ui: slint::Weak::default(),
        session: None,
        audio: None,
        call: None,
        avc,
        runtime: runtime.handle().clone(),
        facing: Facing::Front,
        extra_turns: 0,
        mirror: false,
        resume_camera: false,
        scanning: false,
        scan_busy: Arc::default(),
        contacts,
        connected_at: None,
        stats: Arc::default(),
    }));

    let lifecycle = Rc::clone(&state);
    slint::android::init_with_event_listener(app, move |event| match event {
        // A call keeps the camera: the foreground service is what allows that in the background.
        PollEvent::Main(MainEvent::Pause) => with_state(&lifecycle, |s| {
            s.resume_camera = s.call.is_none() && s.session.take().is_some();
        }),
        PollEvent::Main(MainEvent::Resume { .. }) => with_state(&lifecycle, |s| {
            if std::mem::take(&mut s.resume_camera) {
                s.start_camera();
            }
        }),
        _ => {}
    })?;

    let ui = App::new()?;
    state.borrow_mut().ui = ui.as_weak();
    ui.set_log(report.into());
    ui.set_my_id(identity.to_string().into());
    ui.set_my_fingerprint_lines(fingerprint_lines(&identity));
    ui.set_my_short_fingerprint(groups(&identity, FINGERPRINT_SELF_GROUPS, " · ").into());
    match qr_image(&identity) {
        Ok(image) => ui.set_qr(image),
        Err(e) => tracing::error!("identity qr: {e:#}"),
    }
    show_contacts(&state, &ui);
    slint::spawn_local(handle_node_events(node_events, ui.as_weak(), Rc::clone(&state), Rc::clone(&platform)))?;
    request_notifications(&platform);

    let (c, weak, s) = (calls.clone(), ui.as_weak(), Rc::clone(&state));
    ui.on_call(move |key| match EndpointId::from_str(key.trim()) {
        Ok(peer) => {
            if send_call_command(&c, Command::Call(peer), &weak)
                && let Some(ui) = weak.upgrade()
            {
                // Optimistic: the node confirms with Dialing, or reverts via Ended.
                ui.set_call_state(CallState::Dialing);
                let name = with_contacts(&s, |contacts| contacts.name_of(&peer).map(str::to_owned));
                set_peer(&ui, &name.flatten().unwrap_or_else(|| short(&peer)));
            }
        }
        Err(e) => set_call_status(&weak, format!("that is not a key: {e}")),
    });
    let (c, weak) = (calls.clone(), ui.as_weak());
    ui.on_accept(move || {
        send_call_command(&c, Command::Answer(true), &weak);
    });
    let (c, weak) = (calls.clone(), ui.as_weak());
    ui.on_reject(move || {
        send_call_command(&c, Command::Answer(false), &weak);
    });
    let (c, weak) = (calls, ui.as_weak());
    ui.on_hangup(move || {
        send_call_command(&c, Command::Hangup, &weak);
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
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_mic(move || {
        let muted = with_state_value(&s, State::toggle_mic).unwrap_or_default();
        if let Some(ui) = weak.upgrade() {
            ui.set_mic_on(!muted);
        }
    });
    let (p, weak) = (Rc::clone(&platform), ui.as_weak());
    ui.on_toggle_speaker(move || {
        let Some(ui) = weak.upgrade() else { return };
        let on = !ui.get_speaker_on();
        match p.set_speaker(on) {
            Ok(()) => ui.set_speaker_on(on),
            Err(e) => tracing::warn!("speaker: {e}"),
        }
    });

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_add_contact(move |name, key| {
        let outcome = EndpointId::from_str(key.trim())
            .map_err(|e| format!("that is not a key: {e}"))
            .and_then(|id| save_contact(&s, |contacts| contacts.add(name.trim(), id)));
        match outcome {
            Ok(()) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_peer_key(Default::default());
                    ui.set_new_name(Default::default());
                    show_contacts(&s, &ui);
                }
            }
            Err(e) => set_call_status(&weak, e),
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_rename_contact(move |key, name| {
        if let Ok(id) = EndpointId::from_str(key.trim()) {
            let outcome = save_contact(&s, |contacts| contacts.rename(id, name.trim()));
            match (outcome, weak.upgrade()) {
                (Ok(()), Some(ui)) => show_contacts(&s, &ui),
                (Err(e), _) => set_call_status(&weak, e),
                _ => {}
            }
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_remove_contact(move |key| {
        if let Ok(id) = EndpointId::from_str(key.trim()) {
            let outcome = save_contact(&s, |contacts| contacts.remove_id(id).map(drop));
            match (outcome, weak.upgrade()) {
                (Ok(()), Some(ui)) => show_contacts(&s, &ui),
                (Err(e), _) => set_call_status(&weak, e),
                _ => {}
            }
        }
    });
    let (s, p) = (Rc::clone(&state), Rc::clone(&platform));
    ui.on_pick_key(move || pick_key(&s, &p));
    let (p, weak) = (Rc::clone(&platform), ui.as_weak());
    ui.on_share_key(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Err(e) = p.share_text(&ui.get_my_id(), "My uplink key") {
            tracing::error!("sharing the key: {e}");
            ui.set_call_status(format!("could not share: {e}").into());
        }
    });
    let (s, p, weak) = (Rc::clone(&state), Rc::clone(&platform), ui.as_weak());
    ui.on_scan(move |on| {
        with_state(&s, |s| s.scanning = on);
        if let Some(ui) = weak.upgrade() {
            ui.set_scanning(on);
        }
        // Either way the session is rebuilt: with the scan stream, or without it.
        if on {
            request_camera(&s, &p);
        } else {
            with_state(&s, |s| {
                if s.session.is_some() {
                    s.start_camera();
                }
            });
        }
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
        }
        _ => {}
    })?;

    let stats_timer = Timer::default();
    let s = Rc::clone(&state);
    let mut last = (Instant::now(), cpu::process_seconds());
    stats_timer.start(TimerMode::Repeated, STATS_INTERVAL, move || {
        let now = (Instant::now(), cpu::process_seconds());
        let secs = now.0.duration_since(last.0).as_secs_f64();
        let cpu_percent = last.1.zip(now.1).map(|(before, after)| (after - before) / secs * PERCENT);
        last = now;
        with_state(&s, |state| {
            state.recover_audio();
            let (text, timer) = (stats_text(state, secs, cpu_percent), state.call_timer());
            if let Some(ui) = state.ui.upgrade() {
                ui.set_stats(text.into());
                ui.set_call_timer(timer.into());
            }
        });
    });

    // Everything the first screen needs is wired, so the splash has nothing left to cover. It is
    // brief today because the endpoint starts before the window does; when that moves onto the
    // runtime, this is what the splash will be waiting on.
    ui.set_booting(false);

    let outcome = ui.run();
    if let Err(e) = platform.set_call_service(false) {
        tracing::warn!("stopping call service: {e}");
    }
    {
        let mut state = state.borrow_mut();
        state.session = None;
        state.audio = None;
        state.call = None;
    }
    runtime.block_on(node.shutdown());
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    tracing::info!("exiting");
    Ok(outcome?)
}

fn set_call_status(ui: &slint::Weak<App>, status: String) {
    if let Some(ui) = ui.upgrade() {
        append_log(&ui, &status);
        ui.set_call_status(status.into());
    }
}

/// Returns whether the node accepted the command.
fn send_call_command(calls: &NodeHandle, command: Command, ui: &slint::Weak<App>) -> bool {
    match calls.try_send(command) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(?command, "node command: {e}");
            set_call_status(ui, format!("{command:?} failed: {e}"));
            false
        }
    }
}

/// The peer an event concerns, when it names one.
const fn peer_of(event: &Event) -> Option<EndpointId> {
    match event {
        Event::Dialing { peer } | Event::Ringing { peer } | Event::Incoming { peer } | Event::Connected { peer, .. } => {
            Some(*peer)
        }
        Event::Ready { .. } | Event::Online | Event::Ended { .. } => None,
    }
}

/// UI call state implied by an event; `None` leaves it unchanged.
const fn call_state(event: &Event) -> Option<CallState> {
    match event {
        Event::Ready { .. } | Event::Online => None,
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
        Event::Online => "online".to_owned(),
        Event::Dialing { peer } => format!("dialing {}", peer.fmt_short()),
        Event::Ringing { peer } => format!("ringing {}", peer.fmt_short()),
        Event::Incoming { peer } => format!("incoming call from {}", peer.fmt_short()),
        Event::Connected { peer, key_exchange, .. } => format!("connected to {} [{key_exchange:?}]", peer.fmt_short()),
        Event::Ended { peer, reason } => match peer {
            Some(peer) => format!("call with {} ended: {reason:?}", peer.fmt_short()),
            None => format!("call ended: {reason:?}"),
        },
    }
}

/// Runs on the UI thread (tokio channels work on any executor); applies node events to the UI
/// and starts or stops call video.
async fn handle_node_events(
    mut events: mpsc::Receiver<Event>,
    ui: slint::Weak<App>,
    state: Rc<RefCell<State>>,
    platform: Rc<Platform>,
) {
    while let Some(event) = events.recv().await {
        tracing::info!(?event, "node event");
        let Some(ui) = ui.upgrade() else { break };
        let status = describe(&event);
        append_log(&ui, &status);
        ui.set_call_status(status.into());
        if let Some(call_state) = call_state(&event) {
            ui.set_call_state(call_state);
        }
        // Name whoever is on the other end, by nickname when we know them.
        if let Some(peer) = peer_of(&event) {
            let name = with_contacts(&state, |contacts| contacts.name_of(&peer).map(str::to_owned));
            set_peer(&ui, &name.flatten().unwrap_or_else(|| short(&peer)));
        }
        match event {
            Event::Ready { id } => ui.set_my_id(id.to_string().into()),
            Event::Connected { media, key_exchange, .. } => {
                ui.set_key_exchange(format!("{key_exchange:?}").into());
                with_state(&state, |s| s.connected_at = Some(Instant::now()));
                let MediaSession { video, incoming_video, keyframe_requests, audio, incoming_audio, stats } = *media;
                let parts =
                    VideoParts { sender: video, incoming: incoming_video, keyframe_requests, stats: Arc::clone(&stats) };
                with_state(&state, |s| s.start_video(parts));
                if let Err(e) = platform.set_call_service(true) {
                    tracing::warn!("call service: {e}");
                }
                // (Re)starts the camera with the encoder as a second output.
                request_camera(&state, &platform);
                start_voice(&state, &platform, audio, incoming_audio);
            }
            Event::Ended { .. } => {
                with_state(&state, |s| {
                    s.connected_at = None;
                    s.end_call();
                });
                if let Err(e) = platform.set_call_service(false) {
                    tracing::warn!("stopping call service: {e}");
                }
                if let Err(e) = platform.set_in_call(false) {
                    tracing::warn!("leaving call audio mode: {e}");
                }
            }
            _ => {}
        }
    }
}

/// May run several times per process (Android reuses processes), so nothing here is global:
/// logging and the panic hook are scoped to this call.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from(FALLBACK_DATA_DIR));
    let dispatch = match log::init(LOG_TAG, LOG_FILTER, &data_dir) {
        Ok(dispatch) => dispatch,
        Err(e) => return log::logcat(LOG_TAG, Level::ERROR, &format!("logging init failed: {e}")),
    };
    let _log = tracing::dispatcher::set_default(&dispatch);
    let _panic_hook = PanicHook::install(dispatch.clone());
    if let Err(e) = run(app, &data_dir, dispatch.clone()) {
        tracing::error!("fatal: {e:#}");
    }
}

/// Appends to the in-app log view, keeping the newest [`LOG_VIEW_LINES`] lines.
fn append_log(ui: &App, line: &str) {
    let log = ui.get_log();
    let mut lines: Vec<&str> = log.lines().collect();
    lines.push(line);
    let newest = lines.len().saturating_sub(LOG_VIEW_LINES);
    ui.set_log(lines[newest..].join("\n").into());
}
