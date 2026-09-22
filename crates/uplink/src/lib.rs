//! uplink Android entry point: camera preview through the zero-copy GL path, with
//! in-app diagnostics (previous exits + previous log) since there is no adb.

mod ui;

use std::cell::RefCell;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use slint::android::AndroidApp;
use slint::android::android_activity::{MainEvent, PollEvent};
use slint::{ComponentHandle, RenderingState, Timer, TimerMode};
use uplink_android::camera::{Camera, Facing};
use uplink_android::preview::{Frame, Preview, TURNS_PER_REVOLUTION};
use uplink_android::{cpu, jvm, log};

use crate::ui::App;

const LOG_TAG: &CStr = c"uplink";
/// Baked in at build time (`just log=debug apk`).
const LOG_FILTER: &str = match option_env!("UPLINK_LOG") {
    Some(filter) => filter,
    None => "info",
};
const FALLBACK_DATA_DIR: &str = "/data/local/tmp";
const PREVIOUS_LOG_TAIL_LINES: usize = 20;

const CAPTURE_WIDTH: i32 = 1280;
const CAPTURE_HEIGHT: i32 = 720;
const CAPTURE_FPS: i32 = 30;
const READER_MAX_IMAGES: i32 = 4;
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const PERCENT: f64 = 100.0;

#[derive(Default)]
struct FrameStats {
    camera: AtomicU32,
    blits: AtomicU32,
    blit_micros: AtomicU64,
}

/// Field order is drop order: the shown image, then the camera, then the reader it feeds.
struct Session {
    shown: Option<Image>,
    camera: Camera,
    reader: ImageReader,
}

struct State {
    app: AndroidApp,
    ui: slint::Weak<App>,
    session: Option<Session>,
    facing: Facing,
    extra_turns: i32,
    mirror: bool,
    resume_camera: bool,
    stats: Arc<FrameStats>,
}

impl State {
    fn status(&self, message: impl Into<slint::SharedString>) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_status(message.into());
        }
    }

    fn start_camera(&mut self) {
        match jvm::has_camera_permission(&self.app) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(e) = jvm::request_camera_permission(&self.app) {
                    tracing::error!("requesting camera permission: {e}");
                }
                self.status("camera permission requested; press Start after granting");
                return;
            }
            Err(e) => {
                tracing::error!("checking camera permission: {e}");
                self.status(format!("permission check failed: {e}"));
                return;
            }
        }
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
        let camera = Camera::open(self.facing, &reader.window()?, CAPTURE_FPS)?;
        Ok(Session { shown: None, camera, reader })
    }
}

fn with_state(state: &Rc<RefCell<State>>, f: impl FnOnce(&mut State)) {
    match state.try_borrow_mut() {
        Ok(mut s) => f(&mut s),
        Err(_) => tracing::warn!("state busy; event dropped"),
    }
}

/// Converts the latest camera image into a preview texture, if one arrived.
fn render_frame(state: &mut State, preview: &mut Option<Preview>) -> Result<Option<Frame>> {
    let Some(session) = state.session.as_mut() else { return Ok(None) };
    let AcquireResult::Image(image) = session.reader.acquire_latest_image()? else { return Ok(None) };
    let buffer = image.hardware_buffer()?;
    if preview.is_none() {
        // SAFETY: only called from BeforeRendering, where Slint's GL context is current.
        *preview = Some(unsafe { Preview::new() }?);
    }
    let Some(preview) = preview.as_mut() else { return Ok(None) };
    let turns = session.camera.upright_quarter_turns() + state.extra_turns;
    let mirror = (session.camera.facing() == Facing::Front) ^ state.mirror;
    let (width, height) = (u32::try_from(image.width()?)?, u32::try_from(image.height()?)?);
    // SAFETY: GL context is current; `image` (owner of `buffer`) is kept in `shown` until the
    // next frame replaces it.
    let frame = unsafe { preview.draw(buffer.as_ptr().cast(), width, height, turns, mirror) }?;
    session.shown = Some(image);
    Ok(Some(frame))
}

fn previous_run_report(app: &AndroidApp, data_dir: &Path) -> String {
    let exits = jvm::previous_exits(app).unwrap_or_else(|e| format!("exit info unavailable: {e}"));
    let previous_log = std::fs::read_to_string(data_dir.join(log::PREVIOUS_LOG_FILE)).unwrap_or_default();
    let lines: Vec<&str> = previous_log.lines().collect();
    let tail = lines[lines.len().saturating_sub(PREVIOUS_LOG_TAIL_LINES)..].join("\n");
    format!("previous exits:\n{exits}\nprevious log tail:\n{tail}")
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {info}\n{backtrace}");
    }));
}

fn stats_text(state: &State, secs: f64, cpu_percent: Option<f64>) -> String {
    let camera_fps = f64::from(state.stats.camera.swap(0, Ordering::Relaxed)) / secs;
    let blits = state.stats.blits.swap(0, Ordering::Relaxed);
    let micros = state.stats.blit_micros.swap(0, Ordering::Relaxed);
    let blit_fps = f64::from(blits) / secs;
    let avg_micros = micros.checked_div(u64::from(blits)).unwrap_or_default();
    let cpu = cpu_percent.map_or_else(|| "n/a".to_owned(), |c| format!("{c:.0}%"));
    let camera_error = state.session.as_ref().and_then(|s| s.camera.error());
    tracing::debug!(camera_fps, blit_fps, avg_micros, cpu, "stats");
    format!(
        "cam {camera_fps:.1} fps · blit {blit_fps:.1} fps · {avg_micros} µs/blit · CPU {cpu} (100% = 1 core)\n\
         turns +{} · mirror {} · camera error {camera_error:?}",
        state.extra_turns, state.mirror
    )
}

fn run(app: AndroidApp) -> Result<()> {
    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from(FALLBACK_DATA_DIR));
    log::init(LOG_TAG, LOG_FILTER, &data_dir)?;
    install_panic_hook();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), filter = LOG_FILTER, "starting");
    let report = previous_run_report(&app, &data_dir);

    let state = Rc::new(RefCell::new(State {
        app: app.clone(),
        ui: slint::Weak::default(),
        session: None,
        facing: Facing::Front,
        extra_turns: 0,
        mirror: false,
        resume_camera: false,
        stats: Arc::default(),
    }));

    let lifecycle = Rc::clone(&state);
    slint::android::init_with_event_listener(app, move |event| match event {
        PollEvent::Main(MainEvent::Pause) => with_state(&lifecycle, |s| s.resume_camera = s.session.take().is_some()),
        PollEvent::Main(MainEvent::Resume { .. }) => with_state(&lifecycle, |s| {
            if std::mem::take(&mut s.resume_camera) {
                s.start_camera();
            }
        }),
        _ => {}
    })?;

    let ui = App::new()?;
    state.borrow_mut().ui = ui.as_weak();
    ui.set_status(report.into());

    let s = Rc::clone(&state);
    ui.on_start(move || with_state(&s, State::start_camera));
    let s = Rc::clone(&state);
    ui.on_stop(move || {
        with_state(&s, |s| {
            s.session = None;
            s.status("stopped");
        });
    });
    let s = Rc::clone(&state);
    ui.on_flip(move || {
        with_state(&s, |s| {
            s.facing = s.facing.flipped();
            if s.session.is_some() {
                s.start_camera();
            }
        });
    });
    let s = Rc::clone(&state);
    ui.on_rotate(move || with_state(&s, |s| s.extra_turns = (s.extra_turns + 1) % TURNS_PER_REVOLUTION));
    let s = Rc::clone(&state);
    ui.on_mirror(move || with_state(&s, |s| s.mirror = !s.mirror));

    let s = Rc::clone(&state);
    let mut preview: Option<Preview> = None;
    ui.window().set_rendering_notifier(move |rendering, _| match rendering {
        RenderingState::BeforeRendering => with_state(&s, |state| {
            let started = Instant::now();
            match render_frame(state, &mut preview) {
                Ok(Some(frame)) => {
                    state.stats.blits.fetch_add(1, Ordering::Relaxed);
                    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
                    state.stats.blit_micros.fetch_add(micros, Ordering::Relaxed);
                    // SAFETY: the texture was created on this window's GL context by `Preview`.
                    let image = unsafe {
                        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(frame.texture, (frame.width, frame.height).into())
                    }
                    .build();
                    if let Some(ui) = state.ui.upgrade() {
                        ui.set_frame(image);
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("preview: {e:#}"),
            }
        }),
        RenderingState::RenderingTeardown => {
            if let Some(preview) = preview.take() {
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
            let text = stats_text(state, secs, cpu_percent);
            if let Some(ui) = state.ui.upgrade() {
                ui.set_stats(text.into());
            }
        });
    });

    ui.run()?;
    state.borrow_mut().session = None;
    tracing::info!("exiting");
    Ok(())
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    if let Err(e) = run(app) {
        tracing::error!("fatal: {e:#}");
    }
}
