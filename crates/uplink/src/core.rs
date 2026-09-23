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
use parking_lot::Mutex;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use uplink_core::db::Db;
use uplink_core::node::{Event, Network, Node, NodeHandle};
use uplink_core::{EndpointId, identity};

/// What the app is, minus the looking at it.
///
/// Field order is drop order, and the runtime is last on purpose: the endpoint's tasks run on it,
/// so everything that depends on it has to be gone before it is.
pub struct Core {
    /// Commands in. Cloneable and cheap, which is why the UI never needs the `Node` itself.
    calls: NodeHandle,
    id: EndpointId,
    db: Db,
    /// Events out. Taken by whoever is currently answering for the app — the UI while it is on
    /// screen, and otherwise the headless path. There is only ever one of them.
    events: Mutex<Option<mpsc::Receiver<Event>>>,
    /// Holding this is what keeps the endpoint bound; the handle is what everything else uses.
    node: Mutex<Option<Node>>,
    runtime: Runtime,
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
    pub fn start(runtime: Runtime, data_dir: &Path) -> Result<Self> {
        let secret = runtime.block_on(identity::load_or_create(data_dir))?;
        let id = secret.public();
        let db = Db::open(data_dir)?;
        let (node, events) = runtime.block_on(Node::start(secret, Network::N0))?;
        tracing::info!(%id, "core up");
        Ok(Self {
            calls: node.handle(),
            id,
            db,
            events: Mutex::new(Some(events)),
            node: Mutex::new(Some(node)),
            runtime,
        })
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

    /// Takes the event stream. The second caller gets nothing, which is the honest answer: two
    /// things answering for one endpoint would each see half the calls.
    pub fn take_events(&self) -> Option<mpsc::Receiver<Event>> {
        self.events.lock().take()
    }

    /// Hands the stream back, so the next thing to come along can answer for the app. Called when
    /// the UI goes away, which is exactly when the headless path needs it.
    pub fn return_events(&self, events: mpsc::Receiver<Event>) {
        *self.events.lock() = Some(events);
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
