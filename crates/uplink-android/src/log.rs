//! `tracing` → logcat, plus a file in the app's data dir (readable in-app; no adb).

use std::ffi::{CStr, CString, c_int};
use std::fs::File;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use ndk_sys::android_LogPriority;
use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, SubscriberExt};

use crate::Error;

pub const LOG_FILE: &str = "uplink.log";
pub const PREVIOUS_LOG_FILE: &str = "uplink.prev.log";

/// Builds the subscriber. It is *not* installed globally: `android_main` can run several
/// times per process, so callers scope it (`dispatcher::set_default`) and hand it to other
/// threads. `filter` uses `Targets` syntax, e.g. `info,uplink=debug`. The previous run's log is
/// kept as [`PREVIOUS_LOG_FILE`].
pub fn init(tag: &'static CStr, filter: &str, dir: &Path) -> Result<Dispatch, Error> {
    let current = dir.join(LOG_FILE);
    if current.exists() {
        std::fs::rename(&current, dir.join(PREVIOUS_LOG_FILE))?;
    }
    let file = File::create(current)?;
    let subscriber = tracing_subscriber::registry()
        .with(Logcat { tag })
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(Mutex::new(file)))
        .with(Targets::from_str(filter)?);
    Ok(Dispatch::new(subscriber))
}

/// Writes straight to logcat, for when the subscriber is unavailable.
pub fn logcat(tag: &CStr, level: Level, message: &str) {
    let Ok(text) = CString::new(message.replace('\0', "")) else { return };
    let Ok(prio) = c_int::try_from(priority(level).0) else { return };
    // SAFETY: tag and text are NUL-terminated and outlive the call.
    unsafe { ndk_sys::__android_log_write(prio, tag.as_ptr(), text.as_ptr()) };
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
