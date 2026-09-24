//! The part of uplink that outlives the screen: the tokio runtime, the iroh endpoint, and the
//! stream of things the endpoint has to say.
//!
//! Everything used to hang off `android_main`, which exists only while an activity does. An
//! activity is destroyed whenever the user swipes the app away, and on a reboot there has never
//! been one — so the endpoint went with it and a call had nothing to arrive at. Ownership lives
//! here instead, behind a handle the Java side keeps for as long as the process does. The UI
//! borrows it while it is on screen.
//!
//! No Rust statics: the handle is an `Arc` leaked into a `jlong`, the way `Platform` already
//! hands its own state to Java, and the one process-wide slot holding it is a Java field.
//!
//! It is started from Java, by `UplinkApplication.ensureCore`, whoever needs it first: the window,
//! or the listening service after a boot, an update or a restart. That is why the Application's
//! natives are registered here, in `JNI_OnLoad`, rather than by the window.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime};

use anyhow::Result;
use arc_swap::ArcSwap;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{JNI_ERR, JNI_VERSION_1_6, jint, jlong};
use jni::{EnvUnowned, JavaVM, NativeMethod, Outcome, jni_str};
use parking_lot::Mutex;
use tracing::{Dispatch, Level};
use uplink_android::log::{self, Logging};
use uplink_android::platform::{AppContext, address_from_handle, handle_from_address};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
// By path: `jni::Outcome` is imported above, and the two mean unrelated things.
use uplink_core::calls::{self, CallLog, CallRecord};
use uplink_core::contacts::Contacts;
use uplink_core::db::Db;
use uplink_core::media::MediaStats;
use uplink_core::quality::{Quality, VideoTarget};
use uplink_core::node::{Behind, Command, EndReason, Event, Network, Node, NodeHandle};
use uplink_core::relays::Relays;
use uplink_core::settings::{self, Settings};
use uplink_core::{EndpointId, SecretKey, identity};

use crate::{LOG_FILTER, LOG_TAG};

/// Must match `UplinkApplication.RING_DECLINE`.
const RING_DECLINE: jint = 0;

/// What the app is, minus the looking at it.
///
/// Field order is drop order, and the runtime is last on purpose: the endpoint's tasks run on it,
/// so everything that depends on it has to be gone before it is.
pub struct Core {
    /// Commands in. Cloneable and cheap, which is why the UI never needs the `Node` itself.
    /// Swapped rather than fixed, because rebinding replaces the endpoint underneath it and a
    /// copy taken before that would go on addressing one that has been closed.
    calls: ArcSwap<NodeHandle>,
    id: EndpointId,
    /// Kept so the endpoint can be rebound without becoming somebody else. The key is the
    /// identity: contacts, history and any code already scanned all name it.
    secret: SecretKey,
    db: Db,
    /// Where a window wants events delivered, while there is one. The core consumes the endpoint's
    /// stream itself and forwards through this — rather than handing the stream to whoever is
    /// answering, which had no answer for a process that starts at boot and never has a window.
    inbox: Arc<Mutex<Inbox>>,
    ringer: Ringer,
    /// Holding this is what keeps the endpoint bound; the handle is what everything else uses.
    node: Mutex<Option<Node>>,
    runtime: Runtime,
    /// Last, so it outlives every field above: the runtime's threads log as they wind down, and
    /// the writer stops the moment this drops. It lives here rather than in `android_main`
    /// because that returns when the activity is destroyed — which is exactly when the endpoint
    /// carries on alone and the log becomes the only way to see it.
    logging: Logging,
}

/// Enough for a burst of call state changes; the endpoint never produces them faster than a
/// person can act on them.
const EVENT_QUEUE: usize = 16;

/// Where events go, and the call ringing right now if one is. One lock for both, so a window
/// attaching mid-ring is either sent the ring or already there to see it — never neither.
#[derive(Default)]
struct Inbox {
    window: Option<mpsc::Sender<Event>>,
    ringing: Option<EndpointId>,
}

/// Rings for an incoming call, plays ringback for an outgoing one, and stops either when the call
/// is answered or over, whether or not a window is up. The window cannot own this: the call that
/// most needs ringing is the one nobody is looking at.
#[derive(Clone)]
struct Ringer {
    context: AppContext,
    db: Db,
    /// This build's version, for the notice when one phone is too old for the other.
    app: String,
}

/// The language Android's resources are read in, from the stored setting (`system`, `en`, `ar`):
/// a `values-` folder's code, or empty to follow the phone.
pub fn locale(stored: Option<&str>) -> &'static str {
    match stored {
        Some("en") => "en",
        Some("ar") => "ar",
        _ => "",
    }
}

impl Ringer {
    fn follow(&self, event: &Event) {
        if let Event::Ended { peer: Some(peer), reason: EndReason::Incompatible { behind, theirs } } = event {
            self.update_needed(peer, *behind, theirs);
        }
        let outcome = match event {
            Event::Incoming { peer } => self.context.ring(&self.name_of(peer)),
            // From the moment we dial, not only once their phone rings: someone offline never
            // rings, and silence until the dial times out reads as the app having hung. Starting
            // it again on Ringing does nothing.
            Event::Dialing { .. } | Event::Ringing { .. } => self.context.ringback(),
            Event::Connected { .. } | Event::Ended { .. } => self.context.stop_ringing(),
            _ => return,
        };
        if let Err(e) = outcome {
            tracing::warn!("ringing: {e}");
        }
    }

    /// Nobody answered. The notification only shows when uplink is not in front; Java decides.
    fn missed(&self, peer: &EndpointId) {
        if let Err(e) = self.context.missed_call(&self.name_of(peer)) {
            tracing::warn!("missed-call notification: {e}");
        }
    }

    /// A call that could not happen until one phone updates. As `missed`: only when not in front.
    fn update_needed(&self, peer: &EndpointId, behind: Behind, theirs: &str) {
        if let Err(e) = self.context.update_needed(behind == Behind::Us, &self.name_of(peer), theirs, &self.app) {
            tracing::warn!("update notification: {e}");
        }
    }

    /// The nickname for a saved contact, and the short key for anyone else.
    fn name_of(&self, peer: &EndpointId) -> String {
        let saved = match Contacts::open(self.db.clone()) {
            Ok(contacts) => contacts.name_of(peer).map(str::to_owned),
            Err(e) => {
                tracing::warn!("reading contacts for a ringing call: {e}");
                None
            }
        };
        saved.unwrap_or_else(|| peer.fmt_short().to_string())
    }
}

/// The call in flight, so its outcome is known by the time it ends.
struct Pending {
    peer: EndpointId,
    incoming: bool,
    at: SystemTime,
    connected: Option<Instant>,
    /// The call's own counters, from the moment it connects; read once more when it ends.
    stats: Option<Arc<MediaStats>>,
}

/// Writes every call to the log as it ends, window or no window. A call that rang with nobody
/// looking used to vanish: the log was the window's to write.
struct Ledger {
    log: Option<CallLog>,
    db: Db,
    pending: Option<Pending>,
}

impl Ledger {
    fn open(db: Db) -> Self {
        let log = CallLog::open(db.clone())
            .inspect_err(|e| tracing::error!("opening the call log; calls go unrecorded: {e}"))
            .ok();
        Self { log, db, pending: None }
    }

    /// The record written, when this event ended a call.
    fn follow(&mut self, event: &Event) -> Option<CallRecord> {
        match event {
            Event::Dialing { peer } | Event::Incoming { peer } => {
                let incoming = matches!(event, Event::Incoming { .. });
                self.pending =
                    Some(Pending { peer: *peer, incoming, at: SystemTime::now(), connected: None, stats: None });
                None
            }
            Event::Connected { media, .. } => {
                if let Some(call) = self.pending.as_mut() {
                    call.connected = Some(Instant::now());
                    call.stats = Some(Arc::clone(&media.stats));
                }
                None
            }
            Event::Ended { peer, reason } => {
                // Only the call it names: an end for someone else is not the end of this one.
                if let (Some(ended), Some(call)) = (peer, &self.pending)
                    && *ended != call.peer
                {
                    tracing::warn!(peer = %ended.fmt_short(), "an end for a call that is not the one in progress");
                    return None;
                }
                // An incoming call the two phones could not have never rang, so nothing was noted
                // for it; it is still a call someone tried to make.
                let call = match (self.pending.take(), reason, peer) {
                    (Some(call), ..) => call,
                    (None, EndReason::Incompatible { .. }, Some(peer)) => {
                        Pending { peer: *peer, incoming: true, at: SystemTime::now(), connected: None, stats: None }
                    }
                    (None, ..) => return None,
                };
                let record = CallRecord {
                    peer: call.peer,
                    incoming: call.incoming,
                    outcome: calls::Outcome::of(reason, call.incoming, call.connected.is_some()),
                    at: call.at,
                    duration: call.connected.map(|since| since.elapsed()),
                    traffic: call.stats.as_ref().map(|stats| calls::Traffic {
                        sent: stats.bytes_sent.load(Ordering::Relaxed),
                        received: stats.bytes_received.load(Ordering::Relaxed),
                    }),
                    quality: call.stats.map(|stats| Quality { target: Some(video_target()), ..stats.summary() }),
                };
                self.write(&record);
                Some(record)
            }
            _ => None,
        }
    }

    fn write(&self, record: &CallRecord) {
        if let Some(log) = &self.log
            && let Err(e) = log.record(record)
        {
            tracing::warn!("recording the call: {e}");
        }
        // A contact's second line is the same fact, kept beside it so the list does not have to
        // query the log per row.
        if record.outcome != calls::Outcome::Answered {
            return;
        }
        match Contacts::open(self.db.clone()) {
            Ok(mut contacts) if contacts.contains(&record.peer) => {
                if let Err(e) = contacts.called(record.peer) {
                    tracing::warn!("stamping the call: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("reading contacts to stamp the call: {e}"),
        }
    }
}

/// What this build sets a call's video up to send, as the log keeps it. One setting today; when
/// quality becomes a choice, the one in force for the call goes here instead.
fn video_target() -> VideoTarget {
    const BITS_PER_KBIT: i32 = 1000;
    let unsigned = |value: i32| u32::try_from(value).unwrap_or_default();
    VideoTarget {
        width: unsigned(crate::VIDEO.width),
        height: unsigned(crate::VIDEO.height),
        fps: unsigned(crate::VIDEO.fps),
        kbps: unsigned(crate::VIDEO.bitrate / BITS_PER_KBIT),
    }
}

/// The one consumer of the endpoint's events, for as long as the process lives. A window gets
/// them while it is attached; otherwise they are answered here, because a call arriving at a
/// backgrounded app is the case this whole arrangement exists for.
async fn deliver(mut events: mpsc::Receiver<Event>, inbox: Arc<Mutex<Inbox>>, ringer: Ringer) {
    let mut ledger = Ledger::open(ringer.db.clone());
    while let Some(event) = events.recv().await {
        ringer.follow(&event);
        // Written before the window hears of it, so what it reads back already has this call.
        if let Some(record) = ledger.follow(&event)
            && record.outcome == calls::Outcome::Missed
        {
            ringer.missed(&record.peer);
        }
        // Cloned out rather than held: the lock must not span the await below.
        let window = {
            let mut inbox = inbox.lock();
            match &event {
                Event::Incoming { peer } => inbox.ringing = Some(*peer),
                Event::Connected { .. } | Event::Ended { .. } => inbox.ringing = None,
                _ => {}
            }
            inbox.window.clone()
        };
        let Some(window) = window else {
            unattended(&event);
            continue;
        };
        if let Err(closed) = window.send(event).await {
            // The window went without saying so. Whatever it was, nobody saw it.
            inbox.lock().window.take();
            unattended(&closed.0);
        }
    }
    tracing::info!("endpoint stopped speaking");
}

/// An event with nobody watching. A ringing call has already been handed to the notification,
/// and a window opened from it is sent the call when it attaches.
fn unattended(event: &Event) {
    match event {
        Event::Incoming { peer } => tracing::info!(peer = %peer.fmt_short(), "ringing with no window"),
        event => tracing::debug!(?event, "event with no window"),
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        // Unbound before the runtime goes, because that is where the endpoint's own tasks live.
        // Usually never reached: a process is killed rather than asked to leave.
        if let Some(node) = self.node.lock().take() {
            self.runtime.block_on(node.shutdown());
            tracing::info!("endpoint unbound");
        }
    }
}

impl Core {
    /// Binds the endpoint and opens the database. Blocks until the endpoint is up, because until
    /// it is there is nothing to answer a call with.
    pub fn start(logging: Logging, data_dir: &Path, context: AppContext) -> Result<Self> {
        let runtime = uplink_core::runtime::build(logging.dispatch())?;
        let secret = runtime.block_on(identity::load_or_create(data_dir))?;
        let id = secret.public();
        let db = Db::open(data_dir)?;
        // Read before binding: the relay map is fixed for the life of the endpoint, so a change
        // made on the settings screen lands the next time the process starts.
        let settings = Settings::open(db.clone())?;
        let relays = Relays::load(&settings);
        // Java keeps its own copy for a boot, but the store is what the user last chose.
        if let Err(e) = context.set_language(locale(settings.get(settings::LANGUAGE).as_deref())) {
            tracing::warn!("telling Java the language: {e}");
        }
        // Unknown is said as such rather than failing the start: it only ever appears in a notice.
        let app = context.app_version().unwrap_or_else(|e| {
            tracing::warn!("reading this build's version: {e}");
            String::new()
        });
        let (node, events) = runtime.block_on(Node::start(secret.clone(), Network::Public(relays), &app))?;
        let inbox = Arc::<Mutex<Inbox>>::default();
        let ringer = Ringer { context, db: db.clone(), app };
        runtime.spawn(deliver(events, Arc::clone(&inbox), ringer.clone()));
        tracing::info!(%id, "core up");
        Ok(Self {
            calls: ArcSwap::from_pointee(node.handle()),
            id,
            secret,
            db,
            inbox,
            ringer,
            node: Mutex::new(Some(node)),
            runtime,
            logging,
        })
    }

    /// This build's version, as the other side of a call is told it.
    pub fn app(&self) -> &str {
        &self.ringer.app
    }

    /// For a thread that wants to log through the one subscriber this process has.
    pub fn dispatch(&self) -> Dispatch {
        self.logging.dispatch()
    }

    /// Binds a new endpoint in place of the current one, for a relay map that has changed.
    ///
    /// The identity is the same key, so nothing anyone has saved about us goes stale — only the
    /// path to us does. Any call in progress ends: an endpoint cannot be moved out from under a
    /// live connection, and dropping the media without saying so would be worse.
    ///
    /// The old endpoint is closed before the new one binds, rather than briefly running two on
    /// one key, which is not a thing relays or address lookup would thank us for.
    ///
    /// Async, and meant to be spawned: closing an endpoint and binding another takes long enough
    /// to be felt as a freeze if it is done on the thread that draws.
    pub async fn rebind(&self, relays: Relays) -> Result<()> {
        let previous = self.node.lock().take();
        if let Some(previous) = previous {
            previous.shutdown().await;
        }
        let started = Node::start(self.secret.clone(), Network::Public(relays), &self.ringer.app).await;
        let (node, events) = match started {
            Ok(started) => started,
            // Left with no endpoint at all: say so rather than let the UI keep claiming we are
            // reachable, since the next launch is now the only thing that can fix it.
            Err(e) => {
                if let Some(window) = self.inbox.lock().window.clone() {
                    drop(window.try_send(Event::Offline));
                }
                return Err(e.into());
            }
        };
        self.calls.store(Arc::new(node.handle()));
        self.runtime.spawn(deliver(events, Arc::clone(&self.inbox), self.ringer.clone()));
        *self.node.lock() = Some(node);
        tracing::info!("endpoint rebound");
        Ok(())
    }

    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The current command sender. Loaded each time rather than held: a rebind replaces it, and
    /// whoever cached one would be talking to a closed endpoint.
    pub fn calls(&self) -> Arc<NodeHandle> {
        self.calls.load_full()
    }

    pub const fn id(&self) -> &EndpointId {
        &self.id
    }

    pub fn db(&self) -> Db {
        self.db.clone()
    }

    /// A window says where to send events while it is up. The previous one, if any, stops
    /// receiving: there is one endpoint and one thing showing it at a time.
    ///
    /// A call that started ringing before the window existed is sent to it first: that is the
    /// call it was most likely opened to answer.
    pub fn attach(&self) -> mpsc::Receiver<Event> {
        let (sender, events) = mpsc::channel(EVENT_QUEUE);
        let mut inbox = self.inbox.lock();
        if let Some(peer) = inbox.ringing {
            // A new channel with room in it; there is no way for this to fail.
            drop(sender.try_send(Event::Incoming { peer }));
        }
        inbox.window = Some(sender);
        events
    }

    /// The window has gone. Events go back to being answered without one.
    pub fn detach(&self) {
        self.inbox.lock().window.take();
    }

    /// Leaks this into a `jlong` for the Java side to hold. Reclaimed by [`Self::from_handle`].
    pub fn into_handle(self: Arc<Self>) -> usize {
        Arc::into_raw(self).expose_provenance()
    }

    /// # Safety
    /// `handle` must come from [`Self::into_handle`] and not have been reclaimed.
    pub unsafe fn from_handle(handle: usize) -> Option<Arc<Self>> {
        if handle == 0 {
            return None;
        }
        let pointer = std::ptr::with_exposed_provenance::<Self>(handle);
        // SAFETY: per the caller contract this is a live `Arc<Core>` allocation. The count is
        // raised so the Java side keeps its own reference.
        unsafe {
            Arc::increment_strong_count(pointer);
            Some(Arc::from_raw(pointer))
        }
    }

    /// The core behind a handle Java passed back.
    fn from_java(handle: jlong) -> Option<Arc<Self>> {
        let address = address_from_handle(handle)?;
        // SAFETY: Java only holds the handle `native_start` returned, and never releases it: the
        // core lives as long as the process does.
        unsafe { Self::from_handle(address) }
    }
}

/// Registers `UplinkApplication`'s natives. Runs when the Application loads the library, before
/// any activity, service or receiver exists — and inside a call from our own class, so `FindClass`
/// searches our classloader rather than the system's, which has never heard of us.
#[unsafe(no_mangle)]
extern "system" fn JNI_OnLoad(vm: *mut jni::sys::JavaVM, _reserved: *mut c_void) -> jint {
    // SAFETY: the VM passes a pointer to itself, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(vm) };
    let registered = vm.attach_current_thread(|env| -> Result<(), jni::errors::Error> {
        let class = env.find_class(jni_str!("dev/uplink/UplinkApplication"))?;
        // SAFETY: the function pointers match the Java declarations in UplinkApplication.
        unsafe { env.register_native_methods(&class, &natives()) }
    });
    match registered {
        Ok(()) => JNI_VERSION_1_6,
        Err(e) => {
            log::logcat(LOG_TAG, Level::ERROR, &format!("registering the application's natives: {e}"));
            JNI_ERR
        }
    }
}

fn natives() -> [NativeMethod<'static>; 3] {
    // SAFETY: signatures match the `extern "system"` functions below and UplinkApplication's natives.
    unsafe {
        [
            NativeMethod::from_raw_parts(
                jni_str!("nativeStart"),
                jni_str!("(Ljava/lang/String;)J"),
                native_start as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeLog"),
                jni_str!("(JILjava/lang/String;)V"),
                native_log as *mut c_void,
            ),
            NativeMethod::from_raw_parts(
                jni_str!("nativeRingAction"),
                jni_str!("(JI)V"),
                native_ring_action as *mut c_void,
            ),
        ]
    }
}

/// Starts logging and the core, and returns the handle Java keeps for the life of the process.
/// Zero if it failed, which Java reads as "not listening". Only ever called once per process:
/// `ensureCore` is synchronized and checks first.
extern "system" fn native_start<'local>(
    mut env: EnvUnowned<'local>,
    application: JObject<'local>,
    data_dir: JString<'local>,
) -> jlong {
    let outcome = env.with_env(|env| -> Result<jlong> {
        let data_dir = PathBuf::from(data_dir.try_to_string(env)?);
        let context = AppContext::new(env, &application)?;
        let logging = log::init(LOG_TAG, LOG_FILTER, &data_dir)?;
        // This thread is Java's, with no subscriber of its own, and starting is worth a record.
        let dispatch = logging.dispatch();
        let _log = tracing::dispatcher::set_default(&dispatch);
        let core = Arc::new(Core::start(logging, &data_dir, context)?);
        Ok(handle_from_address(core.into_handle())?)
    });
    // Straight to logcat: if this failed, there may be no subscriber to log to.
    match outcome.into_outcome() {
        Outcome::Ok(handle) => handle,
        Outcome::Err(e) => {
            log::logcat(LOG_TAG, Level::ERROR, &format!("starting the core: {e:#}"));
            0
        }
        Outcome::Panic(_) => {
            log::logcat(LOG_TAG, Level::ERROR, "panic starting the core");
            0
        }
    }
}

/// Java's logs, so they land in the log file too (there is no adb in the field). Java calls in on
/// its own threads, which have no subscriber of their own, so the core's is scoped around it.
extern "system" fn native_log<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    priority: jint,
    message: JString<'local>,
) {
    let Some(core) = Core::from_java(handle) else { return };
    let outcome = env.with_env(|env| -> Result<String, jni::errors::Error> { message.try_to_string(env) });
    let Outcome::Ok(message) = outcome.into_outcome() else { return };
    tracing::dispatcher::with_default(&core.dispatch(), || log::java(priority, &message));
}

/// A button on the ringing notification that needs no window.
extern "system" fn native_ring_action<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    action: jint,
) {
    let Some(core) = Core::from_java(handle) else { return };
    let _log = tracing::dispatcher::set_default(&core.dispatch());
    if action != RING_DECLINE {
        tracing::warn!(action, "unknown ring action");
        return;
    }
    match core.calls().try_send(Command::Answer(false)) {
        Ok(()) => tracing::info!("declined from the notification"),
        Err(e) => tracing::error!("declining from the notification: {e}"),
    }
}
