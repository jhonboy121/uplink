//! `tracing` → logcat, plus a file in the app's data dir (readable in-app; no adb).

use std::ffi::{CStr, CString, c_int};
use std::path::Path;
use std::str::FromStr;

use ndk_sys::android_LogPriority;
use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};
use uplink_core::logs::{self, Rolling};

use crate::Error;

/// What the log files are called; see [`uplink_core::logs`] for how they roll.
pub const LOG_STEM: &str = "uplink";

/// Logging, for as long as whoever holds this keeps it.
///
/// The two belong together and neither is much use alone: the dispatch is where lines go, and
/// the guard is what keeps the thread that writes them alive. Dropping this stops the file log
/// — not just for the dropper, but for every thread still using the dispatch, which is the
/// whole process. So it is held by the thing that lives longest, never by a window.
pub struct Logging {
    dispatch: Dispatch,
    /// Never read. Its `Drop` shuts the writer's worker thread down and flushes what is queued.
    _guard: WorkerGuard,
}

impl Logging {
    /// A handle for another thread to log through. Cheap; they all point at one subscriber.
    pub fn dispatch(&self) -> Dispatch {
        self.dispatch.clone()
    }
}

/// Builds the subscriber. It is *not* installed globally: `android_main` can run several
/// times per process, so callers scope it (`dispatcher::set_default`) and hand it to other
/// threads. `filter` uses `Targets` syntax, e.g. `info,uplink=debug`.
///
/// Call this once per process. A second appender on the same file would write to it from its own
/// thread, against the first one's idea of how large the file has grown.
pub fn init(tag: &'static CStr, filter: &str, dir: &Path) -> Result<Logging, Error> {
    let rolling = Rolling::new(dir, LOG_STEM, logs::MAX_BYTES, logs::KEEP)?;
    // Lossy: under a flood a dropped line beats blocking the thread that logged it, and the
    // threads that log most here are carrying a call.
    let (writer, guard) = tracing_appender::non_blocking(rolling);
    let subscriber = tracing_subscriber::registry()
        .with(Logcat { tag })
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(writer))
        .with(Targets::from_str(filter)?);
    Ok(Logging { dispatch: Dispatch::new(subscriber), _guard: guard })
}

/// Writes straight to logcat, for when the subscriber is unavailable.
pub fn logcat(tag: &CStr, level: Level, message: &str) {
    let Ok(text) = CString::new(message.replace('\0', "")) else { return };
    let Ok(prio) = c_int::try_from(priority(level).0) else { return };
    // SAFETY: tag and text are NUL-terminated and outlive the call.
    unsafe { ndk_sys::__android_log_write(prio, tag.as_ptr(), text.as_ptr()) };
}

/// A line Java logged, re-emitted under the `java` target so it lands in the file too. Java's
/// `android.util.Log` priorities are logcat's own. The caller must have a subscriber in scope:
/// Java calls in on its own threads, which have no default of their own.
pub fn java(priority: c_int, message: &str) {
    let priority = u32::try_from(priority).unwrap_or_default();
    if priority >= android_LogPriority::ANDROID_LOG_ERROR.0 {
        tracing::error!(target: "java", "{message}");
    } else if priority >= android_LogPriority::ANDROID_LOG_WARN.0 {
        tracing::warn!(target: "java", "{message}");
    } else if priority >= android_LogPriority::ANDROID_LOG_INFO.0 {
        tracing::info!(target: "java", "{message}");
    } else {
        tracing::debug!(target: "java", "{message}");
    }
}

struct Logcat {
    tag: &'static CStr,
}

const fn priority(level: Level) -> android_LogPriority {
    match level {
        Level::ERROR => android_LogPriority::ANDROID_LOG_ERROR,
        Level::WARN => android_LogPriority::ANDROID_LOG_WARN,
        Level::INFO => android_LogPriority::ANDROID_LOG_INFO,
        Level::DEBUG => android_LogPriority::ANDROID_LOG_DEBUG,
        Level::TRACE => android_LogPriority::ANDROID_LOG_VERBOSE,
    }
}

impl<S: Subscriber> Layer<S> for Logcat {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut line = format!("{}: ", event.metadata().target());
        event.record(&mut Fields(&mut line));
        logcat(self.tag, *event.metadata().level(), &line);
    }
}

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!("{value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
}
