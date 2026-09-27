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
use jni::objects::{JClass, JObject, JString};
use jni::sys::{JNI_ERR, JNI_VERSION_1_6, jboolean, jint, jlong};
use jni::{EnvUnowned, JavaVM, NativeMethod, Outcome, jni_str};
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tracing::{Dispatch, Level};
use uplink_android::log::{self, Logging};
use uplink_android::platform::{AppContext, Disconnect, TelecomAction, address_from_handle, handle_from_address};
// By path: `jni::Outcome` is imported above, and the two mean unrelated things.
use uplink_core::calls::{self, CallLog, CallRecord};
use uplink_core::contacts::Contacts;
use uplink_core::db::Db;
use uplink_core::media::MediaStats;
use uplink_core::node::{Behind, Command, EndReason, Event, Mode, Network, Node, NodeHandle, RelayView};
use uplink_core::quality::{Quality, VideoTarget};
use uplink_core::reach::Reach;
use uplink_core::settings::{self, Settings};
use uplink_core::{EndpointId, identity};

use crate::{LOG_FILTER, LOG_TAG};

/// Must match `UplinkApplication.RING_DECLINE`.
const RING_DECLINE: jint = 0;

/// What the app is, minus the looking at it.
///
/// Field order is drop order, and the runtime is last on purpose: the endpoint's tasks run on it,
/// so everything that depends on it has to be gone before it is.
pub struct Core {
    /// Commands in. Cloneable and cheap, which is why the UI never needs the `Node` itself.
    calls: NodeHandle,
    id: EndpointId,
    db: Db,
    settings: Settings,
    log: CallLog,
    /// Where a window wants events delivered, while there is one. The core consumes the endpoint's
    /// stream itself and forwards through this — rather than handing the stream to whoever is
    /// answering, which had no answer for a process that starts at boot and never has a window.
    inbox: Arc<Mutex<Inbox>>,
    sending: Sending,
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
    ringing: Option<(EndpointId, Mode)>,
    /// The relays as last reported, for a window that opens after the report.
    relays: Option<RelayView>,
    /// Reachability as last reported. A relay can answer before the window exists, and nothing
    /// says it again until the connection changes.
    reach: Option<Reach>,
}

/// Rings for an incoming call, plays ringback for an outgoing one, and stops either when the call
/// is answered or over, whether or not a window is up. The window cannot own this: the call that
/// most needs ringing is the one nobody is looking at.
///
/// It also keeps Telecom in step with the call, for the same reason: a call Telecom does not know
/// about gets none of the system's audio handling, and one it refuses does not happen.
#[derive(Clone)]
struct Ringer {
    context: AppContext,
    db: Db,
    /// This build's version, for the notice when one phone is too old for the other.
    app: String,
    /// For turning away a call Telecom will not have.
    calls: NodeHandle,
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
    async fn follow(&self, event: &Event) {
        if let Event::Ended { peer: Some(peer), reason: EndReason::Incompatible { behind, theirs } } = event {
            self.update_needed(peer, *behind, theirs).await;
        }
        if let Err(e) = self.telecom(event).await {
            tracing::warn!("telecom: {e}");
        }
        let outcome = match event {
            // From the moment we dial, not only once their phone rings: someone offline never
            // rings, and silence until the dial times out reads as the app having hung. Starting
            // it again on Ringing does nothing. An incoming call rings when Telecom says so.
            Event::Dialing { .. } | Event::Ringing { .. } => self.context.ringback(),
            Event::Connected { .. } | Event::Ended { .. } => self.context.stop_ringing(),
            _ => return,
        };
        if let Err(e) = outcome {
            tracing::warn!("ringing: {e}");
        }
    }

    /// Asked first, before either phone rings: Telecom's no ends the call right there, while the
    /// dial is still looking for them or before the notification exists.
    async fn telecom(&self, event: &Event) -> Result<(), uplink_android::Error> {
        match event {
            Event::Dialing { peer, mode } | Event::Incoming { peer, mode } => {
                let incoming = matches!(event, Event::Incoming { .. });
                if !self.context.telecom_permitted(incoming)? {
                    tracing::warn!(incoming, "telecom will not take a call now");
                    self.refuse();
                    return Ok(());
                }
                let (key, name, video) = (peer.to_string(), self.name_of(peer).await, *mode == Mode::Video);
                if incoming {
                    self.context.telecom_incoming(&key, &name, video)
                } else {
                    self.context.telecom_place(&key, &name, video)
                }
            }
            Event::Connected { .. } => self.context.telecom_active(),
            Event::VideoOn => self.context.telecom_video(true),
            Event::Ended { reason, .. } => self.context.telecom_ended(disconnect(reason)),
            _ => Ok(()),
        }
    }

    fn refuse(&self) {
        if let Err(e) = self.calls.try_send(Command::Refused) {
            tracing::error!("refusing the call: {e}");
        }
    }

    /// Nobody answered. The notification only shows when uplink is not in front; Java decides.
    async fn missed(&self, peer: &EndpointId) {
        if let Err(e) = self.context.missed_call(&self.name_of(peer).await) {
            tracing::warn!("missed-call notification: {e}");
        }
    }

    /// A call that could not happen until one phone updates. As `missed`: only when not in front.
    async fn update_needed(&self, peer: &EndpointId, behind: Behind, theirs: &str) {
        let name = self.name_of(peer).await;
        if let Err(e) = self.context.update_needed(behind == Behind::Us, &name, theirs, &self.app) {
            tracing::warn!("update notification: {e}");
        }
    }

    /// The nickname for a saved contact, and the short key for anyone else.
    async fn name_of(&self, peer: &EndpointId) -> String {
        let saved = match Contacts::saved_name(&self.db, peer).await {
            Ok(name) => name,
            Err(e) => {
                tracing::warn!("reading contacts for a ringing call: {e}");
                None
            }
        };
        saved.unwrap_or_else(|| peer.fmt_short().to_string())
    }
}

/// Why a call ended, as Telecom is told it.
const fn disconnect(reason: &EndReason) -> Disconnect {
    match reason {
        EndReason::LocalHangup => Disconnect::Local,
        EndReason::RemoteHangup | EndReason::Rejected | EndReason::NoAnswer => Disconnect::Remote,
        EndReason::Declined | EndReason::Screened { .. } => Disconnect::Rejected,
        EndReason::Busy => Disconnect::Busy,
        EndReason::DialTimeout
        | EndReason::Incompatible { .. }
        | EndReason::ConnectionLost
        | EndReason::Refused
        | EndReason::Failed(_) => Disconnect::Error,
    }
}

/// The call in flight, so its outcome is known by the time it ends.
struct Pending {
    peer: EndpointId,
    incoming: bool,
    mode: Mode,
    at: SystemTime,
    connected: Option<Instant>,
    /// How far into the call it switched to video, for one placed as voice.
    video_from: Option<std::time::Duration>,
    /// The call's own counters, from the moment it connects; read once more when it ends.
    stats: Option<Arc<MediaStats>>,
}

impl Pending {
    fn new(peer: EndpointId, incoming: bool, mode: Mode) -> Self {
        Self { peer, incoming, mode, at: SystemTime::now(), connected: None, video_from: None, stats: None }
    }
}

/// Writes every call to the log as it ends, window or no window. A call that rang with nobody
/// looking used to vanish: the log was the window's to write.
struct Ledger {
    log: CallLog,
    db: Db,
    pending: Option<Pending>,
    sending: Sending,
}

impl Ledger {
    /// The record written, when this event ended a call.
    async fn follow(&mut self, event: &Event) -> Option<CallRecord> {
        match event {
            Event::Dialing { peer, mode } | Event::Incoming { peer, mode } => {
                let incoming = matches!(event, Event::Incoming { .. });
                self.pending = Some(Pending::new(*peer, incoming, *mode));
                None
            }
            Event::VideoOn => {
                if let Some(call) = self.pending.as_mut() {
                    call.video_from = call.connected.map(|since| since.elapsed());
                }
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
                    (None, EndReason::Incompatible { .. }, Some(peer)) => Pending::new(*peer, true, Mode::Video),
                    // Screened: turned away before ringing, and logged so it can be seen.
                    (None, EndReason::Screened { mode }, Some(peer)) => Pending::new(*peer, true, *mode),
                    (None, ..) => return None,
                };
                let record = CallRecord {
                    peer: call.peer,
                    incoming: call.incoming,
                    outcome: calls::Outcome::of(reason, call.incoming, call.connected.is_some()),
                    mode: call.mode,
                    at: call.at,
                    duration: call.connected.map(|since| since.elapsed()),
                    video_from: call.video_from,
                    traffic: call.stats.as_ref().map(|stats| calls::Traffic {
                        sent: stats.bytes_sent.load(Ordering::Relaxed),
                        received: stats.bytes_received.load(Ordering::Relaxed),
                    }),
                    // Taken, so the next call starts with nothing said about it.
                    quality: call.stats.map(|stats| Quality { target: self.sending.lock().take(), ..stats.summary() }),
                };
                self.write(&record).await;
                Some(record)
            }
            _ => None,
        }
    }

    async fn write(&self, record: &CallRecord) {
        if let Err(e) = self.log.record(record).await {
            tracing::warn!("recording the call: {e}");
        }
        // A contact's second line is the same fact, kept beside it so the list does not have to
        // query the log per row.
        if !record.outcome.answered() {
            return;
        }
        if let Err(e) = Contacts::stamp_called(&self.db, &record.peer).await {
            tracing::warn!("stamping the call: {e}");
        }
    }
}

/// What the call in progress is set up to send, as the window last said: its quality step's
/// picture, which changes when the network does. The log keeps the last one.
type Sending = Arc<Mutex<Option<VideoTarget>>>;

/// The one consumer of the endpoint's events, for as long as the process lives. A window gets
/// them while it is attached; otherwise they are answered here, because a call arriving at a
/// backgrounded app is the case this whole arrangement exists for.
async fn deliver(
    mut events: mpsc::Receiver<Event>,
    inbox: Arc<Mutex<Inbox>>,
    ringer: Ringer,
    log: CallLog,
    sending: Sending,
) {
    let mut ledger = Ledger { log, db: ringer.db.clone(), pending: None, sending };
    while let Some(event) = events.recv().await {
        ringer.follow(&event).await;
        // Written before the window hears of it, so what it reads back already has this call.
        if let Some(record) = ledger.follow(&event).await
            && record.outcome == calls::Outcome::Missed
        {
            ringer.missed(&record.peer).await;
        }
        let window = {
            let mut inbox = inbox.lock();
            match &event {
                Event::Incoming { peer, mode } => inbox.ringing = Some((*peer, *mode)),
                Event::Connected { .. } | Event::Ended { .. } => inbox.ringing = None,
                Event::Relays(view) => inbox.relays = Some(view.clone()),
                Event::Reach(reach) => inbox.reach = Some(*reach),
                _ => {}
            }
            inbox.window.clone()
        };
        let Some(window) = window else {
            unattended(&event);
            continue;
        };
        // Never awaited: a window that stops reading must not stall the endpoint behind it.
        match window.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(event)) => {
                tracing::error!(?event, "window not reading its events, dropped");
            }
            Err(mpsc::error::TrySendError::Closed(event)) => {
                // The window went without saying so. Whatever it was, nobody saw it.
                inbox.lock().window.take();
                unattended(&event);
            }
        }
    }
    tracing::info!("endpoint stopped speaking");
}

/// An event with nobody watching. A ringing call has already been handed to the notification,
/// and a window opened from it is sent the call when it attaches.
fn unattended(event: &Event) {
    match event {
        Event::Incoming { peer, .. } => tracing::info!(peer = %peer.fmt_short(), "ringing with no window"),
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

/// The identity kept by the Android Keystore: the file in the data dir is only ever sealed.
struct Keystore<'a>(&'a AppContext);

const SEALED_FILE: &str = "secret.key.sealed";

impl identity::Vault for Keystore<'_> {
    fn file(&self) -> &'static str {
        SEALED_FILE
    }

    fn seal(&self, secret: &[u8]) -> Result<Vec<u8>, uplink_core::Error> {
        self.0.seal_identity(secret).map_err(|e| uplink_core::Error::Vault(e.to_string()))
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, uplink_core::Error> {
        self.0.open_identity(sealed).map_err(|e| uplink_core::Error::Vault(e.to_string()))
    }
}

impl Core {
    /// Binds the endpoint and opens the database. Blocks until the endpoint is up, because until
    /// it is there is nothing to answer a call with: Java calls this synchronously and holds the
    /// handle it returns. The identity and the database open in the same wait.
    pub fn start(logging: Logging, data_dir: &Path, context: AppContext) -> Result<Self> {
        let runtime = uplink_core::runtime::build(logging.dispatch())?;
        let (secret, db, settings, log) = runtime.block_on(async {
            let secret = identity::load_or_create(data_dir, &Keystore(&context)).await?;
            let db = Db::open(data_dir).await?;
            // Opened once, here, and shared: settings are read from one copy in memory, so the
            // endpoint (the relay pilot, the unknown-caller gate) and every window must hold the
            // same one to see each other's writes. The contacts table is made here too.
            let settings = Settings::open(db.clone()).await?;
            let log = CallLog::open(db.clone()).await?;
            Contacts::open(db.clone()).await?;
            Ok::<_, uplink_core::Error>((secret, db, settings, log))
        })?;
        let id = secret.public();
        // Java keeps its own copy for a boot, but the store is what the user last chose.
        if let Err(e) = context.set_language(locale(settings.get(settings::LANGUAGE).as_deref())) {
            tracing::warn!("telling Java the language: {e}");
        }
        // Unknown is said as such rather than failing the start: it only ever appears in a notice.
        let app = context.app_version().unwrap_or_else(|e| {
            tracing::warn!("reading this build's version: {e}");
            String::new()
        });
        // The relays are read from the store by the endpoint's pilot, which the relay page steers.
        let (node, events) = runtime.block_on(Node::start(secret, Network::Public(settings.clone()), &app))?;
        let inbox = Arc::<Mutex<Inbox>>::default();
        let ringer = Ringer { context, db: db.clone(), app, calls: node.handle() };
        let sending = Sending::default();
        runtime.spawn(deliver(events, Arc::clone(&inbox), ringer.clone(), log.clone(), Arc::clone(&sending)));
        tracing::info!(%id, "core up");
        Ok(Self {
            calls: node.handle(),
            id,
            db,
            settings,
            log,
            inbox,
            sending,
            ringer,
            node: Mutex::new(Some(node)),
            runtime,
            logging,
        })
    }

    /// What the call in progress is set up to send, for its log entry.
    pub fn set_sending(&self, video: Option<VideoTarget>) {
        *self.sending.lock() = video;
    }

    /// This build's version, as the other side of a call is told it.
    pub fn app(&self) -> &str {
        &self.ringer.app
    }

    /// For a thread that wants to log through the one subscriber this process has.
    pub fn dispatch(&self) -> Dispatch {
        self.logging.dispatch()
    }

    pub const fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub const fn calls(&self) -> &NodeHandle {
        &self.calls
    }

    pub const fn id(&self) -> &EndpointId {
        &self.id
    }

    pub fn db(&self) -> Db {
        self.db.clone()
    }

    /// The one copy of the settings, shared with the endpoint.
    pub fn settings(&self) -> Settings {
        self.settings.clone()
    }

    pub fn log(&self) -> CallLog {
        self.log.clone()
    }

    /// A window says where to send events while it is up. The previous one, if any, stops
    /// receiving: there is one endpoint and one thing showing it at a time.
    ///
    /// A call that started ringing before the window existed is sent to it first: that is the
    /// call it was most likely opened to answer.
    pub fn attach(&self) -> mpsc::Receiver<Event> {
        let (sender, events) = mpsc::channel(EVENT_QUEUE);
        let mut inbox = self.inbox.lock();
        // A new channel with room in it for all three; there is no way for these to fail.
        if let Some(reach) = inbox.reach {
            drop(sender.try_send(Event::Reach(reach)));
        }
        if let Some(view) = inbox.relays.clone() {
            drop(sender.try_send(Event::Relays(view)));
        }
        if let Some((peer, mode)) = inbox.ringing {
            drop(sender.try_send(Event::Incoming { peer, mode }));
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

fn natives() -> [NativeMethod<'static>; 6] {
    // SAFETY: signatures match the `extern "system"` functions below and UplinkApplication's natives.
    unsafe {
        [
            NativeMethod::from_raw_parts(jni_str!("nativeInit"), jni_str!("()V"), native_init as *mut c_void),
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
            NativeMethod::from_raw_parts(jni_str!("nativeNetwork"), jni_str!("(JZ)V"), native_network as *mut c_void),
            NativeMethod::from_raw_parts(jni_str!("nativeTelecom"), jni_str!("(JI)V"), native_telecom as *mut c_void),
        ]
    }
}

/// Publishes the VM and the Application to `ndk_context`, which iroh's DNS resolver and netdev
/// read over JNI. Called from `Application.onCreate`, before any activity, service or receiver
/// exists, so a core started at boot sees it too; android-activity's later call with the same
/// Application is ignored (see the patch in external/ndk-context).
extern "system" fn native_init<'local>(mut env: EnvUnowned<'local>, application: JObject<'local>) {
    let outcome = env.with_env(|env| -> Result<(), jni::errors::Error> {
        let vm = env.get_java_vm()?;
        // Never deleted: ndk_context requires it to stay valid until the process exits.
        let application = env.new_global_ref(&application)?.into_raw();
        // SAFETY: the process's VM and a leaked global ref, both valid for the life of the process.
        unsafe { ndk_context::initialize_android_context(vm.get_raw().cast(), application.cast()) };
        Ok(())
    });
    // Logcat: nothing else is up yet.
    match outcome.into_outcome() {
        Outcome::Ok(()) => {}
        Outcome::Err(e) => log::logcat(LOG_TAG, Level::ERROR, &format!("setting the android context: {e}")),
        Outcome::Panic(_) => log::logcat(LOG_TAG, Level::ERROR, "panic setting the android context"),
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

/// What Telecom did with the call: a headset button, the system's own UI, or a refusal. Straight
/// to the endpoint, as the screen's own buttons go, so either means the same thing.
extern "system" fn native_telecom<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    action: jint,
) {
    let Some(core) = Core::from_java(handle) else { return };
    let _log = tracing::dispatcher::set_default(&core.dispatch());
    let Some(action) = TelecomAction::from_code(action) else {
        tracing::warn!(action, "unknown telecom action");
        return;
    };
    let command = match action {
        TelecomAction::Answer => Command::Answer(true),
        TelecomAction::Reject => Command::Answer(false),
        TelecomAction::Hangup => Command::Hangup,
        TelecomAction::Refused => Command::Refused,
    };
    tracing::info!(?action, "from telecom");
    if let Err(e) = core.calls().try_send(command) {
        tracing::error!(?action, "telecom action: {e}");
    }
}

/// Android's default network: a new or changed one (`up`), or none. Java logs what it is.
extern "system" fn native_network<'local>(
    _env: EnvUnowned<'local>,
    _class: JClass<'local>,
    handle: jlong,
    up: jboolean,
) {
    let Some(core) = Core::from_java(handle) else { return };
    let _log = tracing::dispatcher::set_default(&core.dispatch());
    if let Err(e) = core.calls().try_send(Command::Network(up)) {
        tracing::error!("telling the endpoint about the network: {e}");
    }
}
