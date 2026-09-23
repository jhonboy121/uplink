//! uplink Android entry point: calls with camera video through the zero-copy GL path, with
//! in-app diagnostics (previous exits + previous log) since there is no adb.

mod audio;
mod core;
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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::native_window::NativeWindow;
use rustc_hash::FxHashSet;
use slint::android::AndroidApp;
use slint::android::android_activity::{MainEvent, PollEvent};
use slint::{ComponentHandle, RenderingState, Timer, TimerMode};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{Dispatch, Level};
use uplink_android::camera::{Camera, Facing, Intent};
use uplink_android::codec::{Avc, VideoConfig};
use uplink_android::platform::{Permission, Platform, PlatformEvent};
use uplink_android::preview::{Frame, Preview};
use uplink_android::{cpu, log};
use uplink_core::audio::{AudioReceiver, AudioSender};
use uplink_core::calls::{CallLog, CallRecord, Outcome};
use uplink_core::card;
use uplink_core::contacts::Contacts;
use uplink_core::logs;
use uplink_core::settings::{self, Settings};
use uplink_core::qr;
use uplink_core::media::{MediaSession, Route};
use uplink_core::node::{Command, EndReason, Event, NodeHandle};
use uplink_core::EndpointId;

use crate::core::Core;

use crate::audio::CallAudio;
use crate::ui::{
    App, Appearance, CallItem, CallState, Confirm, ContactItem, Grant, PermissionItem, Screen, Theme,
};
use crate::video::{CallVideo, VideoParts};

const LOG_TAG: &CStr = c"uplink";
/// Baked in at build time (`just log=debug apk`).
const LOG_FILTER: &str = match option_env!("UPLINK_LOG") {
    Some(filter) => filter,
    None => "info",
};
const FALLBACK_DATA_DIR: &str = "/data/local/tmp";
/// Stats ticks per line written to the log. The counters are read every second either way.
const STATS_LOG_EVERY: u32 = 5;

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
const QR_PIXELS: usize = 512;
/// Under the code on the shared picture, the name the chooser goes out under, and what the file
/// is called — the key never changes, so one file is rewritten rather than a new one each time.
const SHARE_CAPTION: &str = "Scan to connect";
const SHARE_TITLE: &str = "My uplink code";
const SHARE_FILE: &str = "identity.png";
const DIAGNOSTICS_FILE: &str = "uplink-logs.tar.gz";
const DIAGNOSTICS_TITLE: &str = "uplink diagnostics";
/// Shown on the call screen when the codecs did not come up: the call runs on audio, and saying
/// nothing makes that look like a peer who is sitting still.
const NO_VIDEO: &str = "Video isn't working on this phone — audio only";
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
    log: CallLog,
    /// Contacts ticked for removal. Kept here rather than in the model, which is rebuilt whenever
    /// the list changes and would drop the ticks with it.
    selected: FxHashSet<EndpointId>,
    /// The call in flight, so its outcome is known by the time it ends.
    pending: Option<PendingCall>,
    connected_at: Option<Instant>,
    stats: Arc<FrameStats>,
    /// Commands to the endpoint, which this window borrows rather than owns — the endpoint
    /// belongs to the process and outlives every window it is shown in.
    calls: Option<NodeHandle>,
}

impl State {
    /// Something worth knowing about later. It goes to the log file, which is the only copy
    /// anyone reads — there is no log on screen, because a log on screen never leaves the phone.
    fn status(&self, message: impl AsRef<str>) {
        tracing::info!("{}", message.as_ref());
    }

    /// Assumes the camera permission is granted (see [`request_camera`]).
    fn start_camera(&mut self) {
        self.session = None;
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
        let camera = Camera::open(facing, &windows, CAPTURE_FPS, intent)?;
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
                        // Nothing found is the normal case while the camera hunts, so it is
                        // silent; a code that is not ours is worth saying, once, and scanning
                        // carries on so the next code still gets a chance.
                        let Some(text) = found else { return };
                        let Some(key) = peer_key(&text) else {
                            let _ = ui.upgrade_in_event_loop(|ui| {
                                if ui.get_toast().is_empty() {
                                    toast(&ui, "That code is not an uplink key");
                                }
                            });
                            return;
                        };
                        tracing::info!("scanned a key");
                        // `invoke_scan` runs the same handler the button does, so the camera
                        // rebuilds without this reader.
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            ui.set_peer_key(key.to_string().into());
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
                // Loud on both ends of the report: in the log with the reason, and on the call
                // screen, because the peer cannot tell a broken encoder from a still room.
                tracing::error!("call video: {e:#}");
                self.status(format!("call video failed: {e:#}"));
                self.trouble(NO_VIDEO);
            }
        }
    }

    /// Says on the call screen that part of the call never came up.
    fn trouble(&self, message: &str) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_call_trouble(message.into());
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
                Some(text) => match peer_key(&text) {
                    Some(key) => ui.set_peer_key(key.to_string().into()),
                    None => toast(&ui, "That code is not an uplink key"),
                },
                None => toast(&ui, "No code in that image"),
            },
            Ok(None) => {}
            Err(e) => {
                tracing::error!("picking an image: {e}");
                toast(&ui, format!("Could not open that image: {e}"));
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
async fn write_identity_card(data_dir: PathBuf, key: String) -> Result<()> {
    let directory = Platform::share_dir(&data_dir);
    let card = card::identity(&key, SHARE_CAPTION)?;
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
    title: &'static str,
    trouble: &'static str,
    write: impl Future<Output = Result<()>> + Send + 'static,
) {
    let writing = handle.spawn(write);
    let (platform, weak) = (Rc::clone(platform), ui.clone());
    spawn_ui(async move {
        let shared = match writing.await {
            Ok(Ok(())) => platform.share_file(file, title).map_err(anyhow::Error::from),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(anyhow::Error::from(e)),
        };
        if let (Err(e), Some(ui)) = (shared, weak.upgrade()) {
            tracing::error!("sharing {file}: {e:#}");
            toast(&ui, trouble);
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

/// Contacts for the list, in the order they were added.
fn show_contacts(state: &Rc<RefCell<State>>, ui: &App) {
    let items = with_state_value(state, |s| {
        // The store already orders favourites first, so a group starts wherever the flag changes.
        let mut previous: Option<bool> = None;
        let selected = &s.selected;
        s.contacts
            .iter()
            .map(|contact| {
                let header = match previous {
                    Some(was) if was == contact.favourite => "",
                    _ if contact.favourite => "FAVOURITES",
                    _ => "ALL",
                };
                previous = Some(contact.favourite);
                ContactItem {
                    name: contact.name.clone().into(),
                    id: contact.id.to_string().into(),
                    detail: last_called(contact.last_called).into(),
                    header: header.into(),
                    initial: contact.name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                    tint: 0,
                    favourite: contact.favourite,
                    selected: selected.contains(&contact.id),
                }
            })
            .collect::<Vec<_>>()
    });
    if let Some(items) = items {
        ui.set_contacts(slint::ModelRc::new(slint::VecModel::from(items)));
    }
    let count = with_state_value(state, |s| i32::try_from(s.selected.len()).unwrap_or(i32::MAX));
    ui.set_selected_count(count.unwrap_or_default());
}

/// Fills the contact sheet, and closes it if that key is no longer a contact. Refreshing one that
/// is not open would open it, so a change made elsewhere leaves it alone.
fn refresh_open_contact(state: &Rc<RefCell<State>>, ui: &App, peer: EndpointId) {
    if ui.get_open_contact_id() == peer.to_string().as_str() {
        show_open_contact(state, ui, peer);
    }
}

fn show_open_contact(state: &Rc<RefCell<State>>, ui: &App, peer: EndpointId) {
    let found = with_contacts(state, |contacts| contacts.get(&peer).cloned()).flatten();
    let Some(contact) = found else {
        ui.set_open_contact_id(Default::default());
        return;
    };
    ui.set_open_contact_id(contact.id.to_string().into());
    ui.set_open_contact_initial(contact.name.chars().next().unwrap_or('?').to_uppercase().to_string().into());
    ui.set_open_contact_name(contact.name.into());
    ui.set_open_contact_advertised(contact.advertised.unwrap_or_default().into());
    ui.set_open_contact_favourite(contact.favourite);
    ui.set_open_contact_fingerprint(fingerprint_lines(&peer));
}

/// How many the Calls screen shows; the store keeps more than a screen can use.
const CALLS_SHOWN: i64 = 100;

/// Fills the Calls screen, newest first, grouped by day.
fn show_calls(state: &Rc<RefCell<State>>, ui: &App) {
    let items = with_state_value(state, |s| {
        let records = match s.log.recent(CALLS_SHOWN) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!("reading the call log: {e}");
                return Vec::new();
            }
        };
        let mut previous = String::new();
        records
            .iter()
            .map(|record| {
                let day = day_of(record.at);
                let header = if day == previous { String::new() } else { day.clone() };
                previous = day;
                // A name we chose, else what they called themselves, else the key itself.
                let name = s
                    .contacts
                    .get(&record.peer)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| short(&record.peer));
                CallItem {
                    initial: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
                    name: name.into(),
                    id: record.peer.to_string().into(),
                    detail: describe_call(record).into(),
                    tint: 0,
                    missed: record.outcome == Outcome::Missed,
                    incoming: record.incoming,
                    header: header.into(),
                }
            })
            .collect::<Vec<_>>()
    });
    if let Some(items) = items {
        ui.set_calls(slint::ModelRc::new(slint::VecModel::from(items)));
    }
}

/// "Missed · 20 minutes ago", or "4:12 · Tuesday" for one that was answered.
fn describe_call(record: &CallRecord) -> String {
    let what = match record.outcome {
        Outcome::Answered => match record.duration {
            Some(d) => format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60),
            None => "Answered".to_owned(),
        },
        Outcome::Missed => "Missed".to_owned(),
        Outcome::Declined => "Declined".to_owned(),
        Outcome::Rejected => "They declined".to_owned(),
        Outcome::Cancelled => "Cancelled".to_owned(),
        Outcome::NoAnswer => "No answer".to_owned(),
        Outcome::Unreachable => "Couldn't reach them".to_owned(),
        Outcome::Failed => "Did not connect".to_owned(),
    };
    format!("{what} · {}", clock_of(record.at))
}

/// The day a call happened, as the heading above its group.
fn day_of(at: SystemTime) -> String {
    let Ok(ago) = SystemTime::now().duration_since(at) else {
        return "TODAY".to_owned();
    };
    match ago.as_secs() / 60 / 60 / 24 {
        0 => "TODAY".to_owned(),
        1 => "YESTERDAY".to_owned(),
        days if days < 7 => format!("{days} DAYS AGO"),
        days => format!("{} WEEKS AGO", days / 7),
    }
}

/// Time of day is what a log row wants; the group heading already carries the date.
fn clock_of(at: SystemTime) -> String {
    let Ok(since_epoch) = at.duration_since(UNIX_EPOCH) else {
        return "just now".to_owned();
    };
    let minutes_today = (since_epoch.as_secs() / 60) % (24 * 60);
    format!("{:02}:{:02}", minutes_today / 60, minutes_today % 60)
}

/// A call from the moment it starts until it ends, which is when the log can say what it was.
struct PendingCall {
    peer: EndpointId,
    incoming: bool,
    at: SystemTime,
    answered: bool,
}

/// What an ending means, given which way the call went and whether it was ever picked up.
const fn outcome_of(reason: &EndReason, call: &PendingCall) -> Outcome {
    match reason {
        EndReason::Busy | EndReason::Failed(_) => Outcome::Failed,
        EndReason::DialTimeout => Outcome::Unreachable,
        EndReason::NoAnswer => Outcome::NoAnswer,
        EndReason::Declined => Outcome::Declined,
        EndReason::Rejected => Outcome::Rejected,
        EndReason::LocalHangup | EndReason::RemoteHangup => {
            if call.answered {
                Outcome::Answered
            } else if call.incoming {
                // They rang off, or we never picked up: either way nobody spoke.
                Outcome::Missed
            } else {
                Outcome::Cancelled
            }
        }
    }
}

/// Coarse on purpose: the second line of a contact row answers "recently?", not "when exactly?".
fn last_called(at: Option<SystemTime>) -> String {
    let Some(at) = at else {
        return "Never called".to_owned();
    };
    let Ok(ago) = SystemTime::now().duration_since(at) else {
        return "Called just now".to_owned();
    };
    let minutes = ago.as_secs() / 60;
    let (hours, days) = (minutes / 60, minutes / 60 / 24);
    if minutes < 1 {
        "Called just now".to_owned()
    } else if minutes < 60 {
        format!("Called {minutes} minute{} ago", plural(minutes))
    } else if hours < 24 {
        format!("Called {hours} hour{} ago", plural(hours))
    } else if days < 7 {
        format!("Called {days} day{} ago", plural(days))
    } else {
        let weeks = days / 7;
        format!("Called {weeks} week{} ago", plural(weeks))
    }
}

const fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn with_contacts<T>(state: &Rc<RefCell<State>>, f: impl FnOnce(&Contacts) -> T) -> Option<T> {
    state.try_borrow().ok().map(|s| f(&s.contacts))
}

/// Applies a change, which the store writes as it goes; the error is what the UI shows.
fn save_contact(
    state: &Rc<RefCell<State>>,
    change: impl FnOnce(&mut Contacts) -> Result<(), uplink_core::Error>,
) -> Result<(), String> {
    let mut state = state.try_borrow_mut().map_err(|_| "busy, try again".to_owned())?;
    change(&mut state.contacts).map_err(|e| e.to_string())
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
const GATE: [(Permission, &str, &str); 3] = [
    (Permission::Camera, "Camera", "So they can see you"),
    (Permission::RecordAudio, "Microphone", "So they can hear you"),
    (Permission::PostNotifications, "Notifications", "So you know when someone calls"),
];

fn gate_icon(permission: Permission) -> slint::Image {
    match permission {
        Permission::Camera => slint::Image::load_from_svg_data(include_bytes!("../../../assets/icons/camera.svg")),
        Permission::RecordAudio => slint::Image::load_from_svg_data(include_bytes!("../../../assets/icons/mic.svg")),
        Permission::PostNotifications => {
            slint::Image::load_from_svg_data(include_bytes!("../../../assets/icons/bell.svg"))
        }
    }
    .unwrap_or_default()
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
    for (permission, name, why) in GATE {
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
        items.push(PermissionItem {
            name: name.into(),
            why: why.into(),
            grant,
            icon: gate_icon(permission),
        });
    }
    ui.set_permissions(slint::ModelRc::new(slint::VecModel::from(items)));
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
    if ui.get_confirming() != Confirm::None {
        ui.set_confirming(Confirm::None);
    } else if !ui.get_peer_key().is_empty() {
        ui.set_peer_key(Default::default());
        ui.set_new_name(Default::default());
    } else if !ui.get_open_contact_id().is_empty() {
        ui.set_open_contact_id(Default::default());
    } else if ui.get_scanning() {
        ui.invoke_scan(false);
    } else if ui.get_call_state() != CallState::Idle && !ui.get_call_folded() {
        // A call is never ended by going back — it folds away, as it would from its own control.
        ui.set_call_folded(true);
    } else if ui.get_selecting() {
        ui.invoke_clear_selection();
    } else if ui.get_screen() != Screen::People {
        ui.set_screen(Screen::People);
    } else {
        return false;
    }
    true
}

/// What the call screen calls the route. Empty until a path is known, which is what keeps the
/// marker off the screen rather than showing a guess while the call is still being set up.
const fn route_name(route: Route) -> &'static str {
    match route {
        Route::Unknown => "",
        Route::Direct => "Direct",
        Route::Relay => "Relayed",
    }
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

fn mark_gate_explained(settings: &Settings) {
    if let Err(e) = settings.set_flag(settings::GATE_EXPLAINED, true) {
        tracing::warn!("recording that the gate was explained: {e}");
    }
}

/// Asks for each missing permission in turn, then re-reads the gate.
fn request_gate(ui: &App, platform: &Rc<Platform>) {
    let (weak, platform) = (ui.as_weak(), Rc::clone(platform));
    let task = slint::spawn_local(async move {
        for (permission, _, _) in GATE {
            match platform.request_permission(permission).await {
                Ok(granted) => tracing::info!(?permission, granted, "gate"),
                Err(e) => tracing::warn!(?permission, "gate: {e}"),
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_asked(true);
            refresh_gate(&ui, &platform);
        }
    });
    if let Err(e) = task {
        tracing::error!("spawning the permission gate: {e}");
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
    let (platform, platform_events) = Platform::attach(&app)?;
    let platform = Rc::new(platform);
    log_previous_exits(&platform);
    let avc = platform.avc()?;

    // The endpoint belongs to the process, not to this window. A second launch of the activity
    // finds the one already bound rather than binding another.
    // SAFETY: the handle is one this process leaked below and Java has held ever since.
    let core = match unsafe { Core::from_handle(platform.core_handle()?) } {
        Some(core) => core,
        None => {
            let core = Arc::new(Core::start(uplink_core::runtime::build(dispatch)?, data_dir)?);
            platform.set_core_handle(Arc::clone(&core).into_handle())?;
            core
        }
    };
    // Only once the endpoint is really up, so the notification never claims a readiness the app
    // does not have.
    if let Err(e) = platform.set_listening(true) {
        tracing::warn!("staying reachable: {e}");
    }
    let identity = *core.id();
    // Three views of the core's one connection; each creates its own table on top of it.
    let contacts = Contacts::open(core.db())?;
    let log = CallLog::open(core.db())?;
    let settings = Settings::open(core.db())?;
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
        scanning: false,
        scan_busy: Arc::default(),
        contacts,
        log,
        selected: FxHashSet::default(),
        pending: None,
        connected_at: None,
        stats: Arc::default(),
        calls: None,
    }));

    let lifecycle = Rc::clone(&state);
    let resumed = Rc::downgrade(&platform);
    slint::android::init_with_event_listener(app, move |event| match event {
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
            }
        }),
        _ => {}
    })?;

    let ui = App::new()?;
    state.borrow_mut().ui = ui.as_weak();
    ui.set_my_id(identity.to_string().into());
    ui.set_my_fingerprint_lines(fingerprint_lines(&identity));
    ui.set_my_short_fingerprint(groups(&identity, FINGERPRINT_SELF_GROUPS, " · ").into());
    match qr_image(&identity) {
        Ok((image, mark)) => {
            ui.set_qr(image);
            ui.set_qr_mark(mark);
        }
        Err(e) => tracing::error!("identity qr: {e:#}"),
    }
    show_contacts(&state, &ui);
    show_calls(&state, &ui);

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_toggle_selected(move |id| {
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        with_state(&s, |state| {
            if !state.selected.remove(&peer) {
                state.selected.insert(peer);
            }
        });
        if let Some(ui) = weak.upgrade() {
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
        let mut failed = 0;
        for peer in &chosen {
            if save_contact(&s, |contacts| contacts.remove_id(*peer).map(drop)).is_err() {
                failed += 1;
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_selecting(false);
            show_contacts(&s, &ui);
            if failed > 0 {
                toast(&ui, format!("{failed} of {} could not be removed", chosen.len()));
            }
        }
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_open_contact(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        show_open_contact(&s, &ui, peer);
    });
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_set_favourite(move |id, favourite| {
        let Ok(peer) = EndpointId::from_str(id.trim()) else { return };
        let outcome = save_contact(&s, |contacts| contacts.set_favourite(peer, favourite));
        if let Some(ui) = weak.upgrade() {
            match outcome {
                Ok(()) => {
                    show_contacts(&s, &ui);
                    refresh_open_contact(&s, &ui, peer);
                }
                Err(e) => set_call_status(&ui.as_weak(), e),
            }
        }
    });

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_clear_calls(move || {
        with_state(&s, |state| {
            if let Err(e) = state.log.clear() {
                tracing::warn!("clearing the call log: {e}");
            }
        });
        if let Some(ui) = weak.upgrade() {
            show_calls(&s, &ui);
        }
    });

    // Anything missing on a launch that has already seen the explainer goes straight to Android's
    // dialog: a lapsed one-time grant should not mean reading the same screen again.
    if !refresh_gate(&ui, &platform) && settings.flag(settings::GATE_EXPLAINED) {
        request_gate(&ui, &platform);
    }

    let (weak, p, s) = (ui.as_weak(), Rc::clone(&platform), settings.clone());
    ui.on_grant_permissions(move || {
        if let Some(ui) = weak.upgrade() {
            mark_gate_explained(&s);
            request_gate(&ui, &p);
        }
    });
    let (weak, p) = (ui.as_weak(), Rc::clone(&platform));
    ui.on_open_settings(move || {
        if let Err(e) = p.open_app_settings() {
            set_call_status(&weak, format!("could not open settings: {e}"));
        }
    });

    // What the user chose last time, before anything can report a change back.
    let theme = ui.global::<Theme>();
    theme.set_appearance(appearance_from(settings.get(settings::APPEARANCE).as_deref()));
    theme.set_rtl(settings.flag(settings::LAYOUT_RTL));

    let s = settings.clone();
    ui.on_appearance_changed(move |appearance| {
        if let Err(e) = s.set(settings::APPEARANCE, appearance_name(appearance)) {
            tracing::warn!("storing the appearance: {e}");
        }
    });
    let s = settings.clone();
    ui.on_rtl_changed(move |rtl| {
        if let Err(e) = s.set_flag(settings::LAYOUT_RTL, rtl) {
            tracing::warn!("storing the layout direction: {e}");
        }
    });

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
    let (weak, mut events) = (ui.as_weak(), platform_events);
    spawn_ui(async move {
        while let Some(event) = events.recv().await {
            let Some(ui) = weak.upgrade() else { break };
            match event {
                PlatformEvent::PictureInPicture(active) => ui.set_call_pip(active),
                PlatformEvent::Hangup => ui.invoke_hangup(),
                PlatformEvent::ToggleMic => ui.invoke_toggle_mic(),
            }
        }
    });

    // The endpoint is already bound by the time the window exists, so the splash only has to last
    // as long as the first frame.
    state.borrow_mut().calls = Some(core.calls().clone());
    ui.set_booting(false);
    // While this window is up it answers for the app; handing the stream back on the way out is
    // what lets anything else take over.
    let (s, weak, p, c) = (Rc::clone(&state), ui.as_weak(), Rc::clone(&platform), Arc::clone(&core));
    spawn_ui(async move {
        let Some(events) = c.take_events() else {
            tracing::warn!("something else is already answering for this endpoint");
            return;
        };
        let events = handle_node_events(events, weak, s, p).await;
        c.return_events(events);
    });

    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_call(move |key| match EndpointId::from_str(key.trim()) {
        Ok(peer) => {
            if send_call_command(&s, Command::Call(peer), &weak)
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
    let (s, weak) = (Rc::clone(&state), ui.as_weak());
    ui.on_accept(move || {
        send_call_command(&s, Command::Answer(true), &weak);
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
        // The picture-in-picture window carries its own mute button; it has to agree.
        if let Err(e) = p.set_mic_on(!muted) {
            tracing::warn!("mic state for the call window: {e}");
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
                (Ok(()), Some(ui)) => {
                    show_contacts(&s, &ui);
                    // The sheet is still open on this contact, so it re-reads too — otherwise it
                    // keeps showing the old name until it is closed and opened again.
                    refresh_open_contact(&s, &ui, id);
                }
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
    let (p, weak, dir) = (Rc::clone(&platform), ui.as_weak(), data_dir.to_path_buf());
    let handle = runtime.clone();
    ui.on_share_key(move || {
        let Some(ui) = weak.upgrade() else { return };
        // The picture, not the key: a code is what someone points a camera at, and it is what
        // arrives if they save it and open it from the other side. Drawing and writing it are
        // the runtime's work, not this tap's.
        let card = write_identity_card(dir.clone(), ui.get_my_id().to_string());
        share_when_written(&handle, &p, &weak, SHARE_FILE, SHARE_TITLE, "Could not share your code", card);
    });
    let (p, weak, dir, handle) = (Rc::clone(&platform), ui.as_weak(), data_dir.to_path_buf(), runtime.clone());
    ui.on_share_diagnostics(move || {
        // The set can be a hundred megabytes before it compresses; packing it is the runtime's.
        let packing = pack_diagnostics(dir.clone());
        let trouble = "Could not pack the log";
        share_when_written(&handle, &p, &weak, DIAGNOSTICS_FILE, DIAGNOSTICS_TITLE, trouble, packing);
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
    let mut ticks: u32 = 0;
    stats_timer.start(TimerMode::Repeated, STATS_INTERVAL, move || {
        let now = (Instant::now(), cpu::process_seconds());
        let secs = now.0.duration_since(last.0).as_secs_f64();
        let cpu_percent = last.1.zip(now.1).map(|(before, after)| (after - before) / secs * PERCENT);
        last = now;
        ticks += 1;
        with_state(&s, |state| {
            state.recover_audio();
            // A codec that dies mid-call takes the picture with it and says nothing otherwise.
            if state.call.as_ref().is_some_and(|call| !call.codecs_running()) {
                state.trouble(NO_VIDEO);
            }
            // Counted every second, written every few: the log is read by whoever is fixing a
            // call that has already happened, and a line a second would bury it.
            let text = stats_text(state, secs, cpu_percent);
            if ticks.is_multiple_of(STATS_LOG_EVERY) {
                tracing::info!("{text}");
            }
            let route = state.call.as_ref().map_or(Route::Unknown, |call| call.stats.route());
            if let Some(ui) = state.ui.upgrade() {
                ui.set_call_timer(state.call_timer().into());
                ui.set_call_route(route_name(route).into());
            }
        });
    });

    let outcome = ui.run();
    if let Err(e) = platform.set_call_service(false, "") {
        tracing::warn!("stopping call service: {e}");
    }
    // Only what this window owned. The endpoint and the runtime stay: they belong to the process,
    // and tearing them down here is exactly what used to make a swiped-away app unreachable.
    {
        let mut state = state.borrow_mut();
        state.session = None;
        state.audio = None;
        state.call = None;
        state.calls = None;
    }
    tracing::info!("window closed, endpoint still bound");
    Ok(outcome?)
}

/// Says something went wrong and gets out of the way; the markup's own Timer dismisses it.
fn toast(ui: &App, message: impl Into<String>) {
    let message = message.into();
    tracing::info!("{message}");
    ui.set_toast(message.into());
}

/// A scanned or opened code only counts if it is actually one of our keys; anything else is some
/// other app's QR and saying so beats silently filling the field with it.
fn peer_key(text: &str) -> Option<EndpointId> {
    EndpointId::from_str(text.trim()).ok()
}

fn set_call_status(ui: &slint::Weak<App>, status: String) {
    tracing::info!("{status}");
    if let Some(ui) = ui.upgrade() {
        ui.set_call_status(status.into());
    }
}

/// Returns whether the node accepted the command. The endpoint may not exist yet, since it binds
/// while the window is already showing.
fn send_call_command(state: &Rc<RefCell<State>>, command: Command, ui: &slint::Weak<App>) -> bool {
    let Some(calls) = state.borrow().calls.clone() else {
        set_call_status(ui, "still starting up".into());
        return false;
    };
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
        Event::Ready { .. } | Event::Online | Event::Offline | Event::Ended { .. } => None,
    }
}

/// UI call state implied by an event; `None` leaves it unchanged.
const fn call_state(event: &Event) -> Option<CallState> {
    match event {
        Event::Ready { .. } | Event::Online | Event::Offline => None,
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
        Event::Online => "reachable".to_owned(),
        Event::Offline => "not reachable".to_owned(),
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
/// and starts or stops call video. Returns the stream when this window stops answering for the
/// app, so whatever comes next can pick it up.
async fn handle_node_events(
    mut events: mpsc::Receiver<Event>,
    ui: slint::Weak<App>,
    state: Rc<RefCell<State>>,
    platform: Rc<Platform>,
) -> mpsc::Receiver<Event> {
    while let Some(event) = events.recv().await {
        let status = describe(&event);
        tracing::info!("{status}");
        let Some(ui) = ui.upgrade() else { break };
        ui.set_call_status(status.into());
        if let Some(call_state) = call_state(&event) {
            ui.set_call_state(call_state);
        }
        // Name whoever is on the other end, by nickname when we know them.
        if let Some(peer) = peer_of(&event) {
            let name = with_contacts(&state, |contacts| contacts.name_of(&peer).map(str::to_owned));
            set_peer(&ui, &name.flatten().unwrap_or_else(|| short(&peer)));
        }
        // A call starts here and is only written to the log once it ends, because until then
        // there is no outcome to write.
        match &event {
            Event::Dialing { peer } | Event::Incoming { peer } => {
                let incoming = matches!(event, Event::Incoming { .. });
                // Whatever went wrong last time was about last time.
                ui.set_call_trouble(Default::default());
                with_state(&state, |s| {
                    s.pending = Some(PendingCall { peer: *peer, incoming, at: SystemTime::now(), answered: false });
                });
            }
            Event::Connected { .. } => {
                with_state(&state, |s| {
                    if let Some(call) = s.pending.as_mut() {
                        call.answered = true;
                    }
                });
            }
            Event::Ended { reason, .. } => {
                let reason = reason.clone();
                with_state(&state, |s| {
                    let Some(call) = s.pending.take() else { return };
                    let duration = s.connected_at.map(|since| since.elapsed());
                    let record = CallRecord {
                        peer: call.peer,
                        incoming: call.incoming,
                        outcome: outcome_of(&reason, &call),
                        at: call.at,
                        duration: call.answered.then_some(duration).flatten(),
                    };
                    if let Err(e) = s.log.record(&record) {
                        tracing::warn!("recording the call: {e}");
                    }
                    // A contact's second line is the same fact, kept beside it so the list does
                    // not have to query the log per row.
                    if record.outcome == Outcome::Answered
                        && s.contacts.contains(&call.peer)
                        && let Err(e) = s.contacts.called(call.peer)
                    {
                        tracing::warn!("stamping the call: {e}");
                    }
                });
                show_calls(&state, &ui);
                show_contacts(&state, &ui);
            }
            _ => {}
        }
        match event {
            Event::Ready { id } => ui.set_my_id(id.to_string().into()),
            Event::Online => ui.set_online(true),
            Event::Offline => ui.set_online(false),
            Event::Connected { media, key_exchange, .. } => {
                ui.set_key_exchange(format!("{key_exchange:?}").into());
                with_state(&state, |s| s.connected_at = Some(Instant::now()));
                let MediaSession { video, incoming_video, keyframe_requests, audio, incoming_audio, stats } = *media;
                let parts =
                    VideoParts { sender: video, incoming: incoming_video, keyframe_requests, stats: Arc::clone(&stats) };
                with_state(&state, |s| s.start_video(parts));
                // The peer is already named on the window; the notification names them too.
                if let Err(e) = platform.set_call_service(true, &ui.get_peer_name()) {
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
                if let Err(e) = platform.set_call_service(false, "") {
                    tracing::warn!("stopping call service: {e}");
                }
                if let Err(e) = platform.set_in_call(false) {
                    tracing::warn!("leaving call audio mode: {e}");
                }
            }
            _ => {}
        }
    }
    events
}

/// May run several times per process (Android reuses processes), so nothing here is global:
/// logging and the panic hook are scoped to this call.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from(FALLBACK_DATA_DIR));
    // The guard flushes the log writer's worker; logging stops the moment it is dropped.
    let (dispatch, _writer) = match log::init(LOG_TAG, LOG_FILTER, &data_dir) {
        Ok(logging) => logging,
        Err(e) => return log::logcat(LOG_TAG, Level::ERROR, &format!("logging init failed: {e}")),
    };
    let _log = tracing::dispatcher::set_default(&dispatch);
    let _panic_hook = PanicHook::install(dispatch.clone());
    if let Err(e) = run(app, &data_dir, dispatch.clone()) {
        tracing::error!("fatal: {e:#}");
    }
}

