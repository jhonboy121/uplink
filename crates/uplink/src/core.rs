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

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use tracing::Dispatch;
use uplink_android::log::Logging;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use uplink_core::db::Db;
use uplink_core::node::{Event, Network, Node, NodeHandle};
use uplink_core::relays::Relays;
use uplink_core::settings::Settings;
use uplink_core::{EndpointId, SecretKey, identity};

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
    inbox: Arc<Mutex<Option<mpsc::Sender<Event>>>>,
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

/// The one consumer of the endpoint's events, for as long as the process lives. A window gets
/// them while it is attached; otherwise they are answered here, because a call arriving at a
/// backgrounded app is the case this whole arrangement exists for.
async fn deliver(mut events: mpsc::Receiver<Event>, inbox: Arc<Mutex<Option<mpsc::Sender<Event>>>>) {
    while let Some(event) = events.recv().await {
        // Cloned out rather than held: the lock must not span the await below.
        let window = inbox.lock().clone();
        let Some(window) = window else {
            unattended(event);
            continue;
        };
        if let Err(closed) = window.send(event).await {
            // The window went without saying so. Whatever it was, nobody saw it.
            inbox.lock().take();
            unattended(closed.0);
        }
    }
    tracing::info!("endpoint stopped speaking");
}

/// What happens to an event with nobody watching. Ringing lives here, and until it does an
/// incoming call is logged and nothing else — which is worth saying out loud rather than
/// dropping it silently.
fn unattended(event: Event) {
    match event {
        Event::Incoming { peer } => tracing::warn!(peer = %peer.fmt_short(), "call with no window to show it"),
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
    pub fn start(logging: Logging, data_dir: &Path) -> Result<Self> {
        let runtime = uplink_core::runtime::build(logging.dispatch())?;
        let secret = runtime.block_on(identity::load_or_create(data_dir))?;
        let id = secret.public();
        let db = Db::open(data_dir)?;
        // Read before binding: the relay map is fixed for the life of the endpoint, so a change
        // made on the settings screen lands the next time the process starts.
        let relays = Relays::load(&Settings::open(db.clone())?);
        let (node, events) = runtime.block_on(Node::start(secret.clone(), Network::Public(relays)))?;
        let inbox = Arc::<Mutex<Option<mpsc::Sender<Event>>>>::default();
        runtime.spawn(deliver(events, Arc::clone(&inbox)));
        tracing::info!(%id, "core up");
        Ok(Self {
            calls: ArcSwap::from_pointee(node.handle()),
            id,
            secret,
            db,
            inbox,
            node: Mutex::new(Some(node)),
            runtime,
            logging,
        })
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
        let started = Node::start(self.secret.clone(), Network::Public(relays)).await;
        let (node, events) = match started {
            Ok(started) => started,
            // Left with no endpoint at all: say so rather than let the UI keep claiming we are
            // reachable, since the next launch is now the only thing that can fix it.
            Err(e) => {
                if let Some(window) = self.inbox.lock().clone() {
                    drop(window.try_send(Event::Offline));
                }
                return Err(e.into());
            }
        };
        self.calls.store(Arc::new(node.handle()));
        self.runtime.spawn(deliver(events, Arc::clone(&self.inbox)));
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
    pub fn attach(&self) -> mpsc::Receiver<Event> {
        let (sender, events) = mpsc::channel(EVENT_QUEUE);
        *self.inbox.lock() = Some(sender);
        events
    }

    /// The window has gone. Events go back to being answered without one.
    pub fn detach(&self) {
        self.inbox.lock().take();
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
}
