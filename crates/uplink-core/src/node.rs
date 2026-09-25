//! Call engine: owns the iroh endpoint and handles one call at a time.
//!
//! [`Node::start`] binds the endpoint and returns the node (commands in) plus an event receiver.
//! Each call runs in its own task; signals are read by a separate reader task so `select!`
//! never cancels a half-read frame.
//!
//! A connected call outlives its connection: when the network drops it, the side with the lower
//! key re-dials with an offer naming the call, the other side's engine hands that to the call
//! instead of answering busy, and the media carries on over the new connection.

use std::hash::{BuildHasher, RandomState};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::{Connection, ConnectionError, Incoming, QuicTransportConfig, SendStream, VarInt, presets};
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, SecretKey, TransportAddr, Watcher as _};
use rustls::NamedGroup;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::media::{self, MediaLinks, MediaSession, MediaStats};
use crate::pilot::Pilot;
pub use crate::pilot::{RelayView, Steer};
/// Part of [`EndReason::Incompatible`], so it is ours to hand out; the rest of the wire stays in.
pub use crate::protocol::Behind;
/// What each side says about its mic and camera; the app sends ours and is told theirs.
pub use crate::protocol::MediaState;
use crate::protocol::{
    self, ALPN, CLOSE_BUSY, CLOSE_HANGUP, CLOSE_INCOMPATIBLE, CLOSE_NOT_POST_QUANTUM, CLOSE_PROTOCOL, CLOSE_REJECTED,
    CLOSE_REJOINED, Hello, Setup, Signal,
};
use crate::reach::{Reach, Reachability};
use crate::settings::Settings;
use crate::{EndpointId, Error, crypto};

const COMMAND_QUEUE: usize = 16;
const EVENT_QUEUE: usize = 64;
const CONTROL_QUEUE: usize = 4;
const SIGNAL_QUEUE: usize = 4;
/// Time the peer gets to close the connection after our final signal.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Time an active call gets to hang up cleanly when the node shuts down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// How long to keep trying to reach a peer before giving up on the key entirely.
const DIAL_TIMEOUT: Duration = Duration::from_secs(20);
/// How long their phone rings before we stop waiting. Longer than dialling, because this one is
/// a person deciding rather than a network failing.
const RING_TIMEOUT: Duration = Duration::from_secs(45);
/// How often the endpoint says what it has been doing. Long enough that the line costs nothing
/// over a night, short enough to place a stall within the log.
const BEAT: Duration = Duration::from_secs(300);
/// How long a call whose connection dropped keeps trying to get it back before it ends.
const RESUME_GRACE: Duration = Duration::from_secs(30);
/// Between two attempts to re-dial a dropped call.
const REDIAL_PAUSE: Duration = Duration::from_secs(1);
/// How long a connection arriving mid-call gets to say whether it is that call coming back.
const OFFER_WAIT: Duration = Duration::from_secs(10);
/// A call's connection is never quiet for longer than this, muted with the camera off or not, so
/// a longer silence is the network rather than the people. It costs a packet a second, and only
/// on a connection that exists: an idle endpoint has none.
pub(crate) const KEEP_ALIVE: Duration = Duration::from_secs(1);
/// Frame streams the peer may have open to us at once. A frame's lasts until delivered, or its
/// deadline and a round trip: 60 fps over a second-long round trip is ~90, over QUIC's default
/// of 100, and a sender out of stream credit cannot send even the newest frame.
const FRAME_STREAMS: VarInt = VarInt::from_u32(256);

/// How a call is placed: with the camera, or voice alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Video,
    Voice,
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Call(EndpointId, Mode),
    Answer(bool),
    /// The platform will not have this call (an emergency call is up, say): one ringing here is
    /// turned away as busy, which is not the user saying no, and any other is hung up.
    Refused,
    Hangup,
    /// Our mic or camera changed; the other side is told.
    Media(MediaState),
    /// Asks the other side to switch this voice call to video, or (false) takes the ask back.
    AskVideo(bool),
    /// Answers their ask to switch to video.
    AnswerVideo(bool),
    /// For the relay pilot; ignored on a local network, which has none.
    Relays(Steer),
    /// The platform's network: up (a new one, or the same one changed) or gone. iroh cannot see
    /// this on Android, and until told keeps the old network's DNS servers.
    Network(bool),
}

#[derive(Debug)]
pub enum Event {
    Ready {
        id: EndpointId,
    },
    /// Whether others can reach us, for the chip: see [`crate::reach`].
    Reach(Reach),
    /// The platform's network changed, as [`Command::Network`] said. A stalled call right after
    /// this is our side's doing, which the chip's patience would hide.
    Network(bool),
    Dialing {
        peer: EndpointId,
        mode: Mode,
    },
    /// Our offer reached the peer; waiting for them to answer.
    Ringing {
        peer: EndpointId,
    },
    Incoming {
        peer: EndpointId,
        mode: Mode,
    },
    /// Always post-quantum: other key exchanges are refused.
    Connected {
        peer: EndpointId,
        key_exchange: NamedGroup,
        mode: Mode,
        media: Box<MediaSession>,
    },
    /// Their mic or camera changed.
    PeerMedia(MediaState),
    /// They ask to switch to video, or (false) took the ask back.
    VideoAsked(bool),
    /// Both sides agreed: the call is video from here on.
    VideoOn,
    /// They kept it voice.
    VideoDeclined,
    /// The connection is gone and the call is trying to get it back.
    Reconnecting,
    /// Back, over a new connection; the media carries on by itself.
    Reconnected,
    Ended {
        peer: Option<EndpointId>,
        reason: EndReason,
    },
    /// The relays: the last survey, what iroh is using, and which one we are reachable through.
    Relays(RelayView),
}

#[derive(Clone, Debug)]
pub enum EndReason {
    LocalHangup,
    RemoteHangup,
    /// The peer declined our call.
    Rejected,
    /// We declined their call.
    Declined,
    Busy,
    /// Never reached them: no listener on that key, or no route to it.
    DialTimeout,
    /// Reached them and rang, but nobody picked up.
    NoAnswer,
    /// One side needs something the other cannot do, so the two cannot call until one updates.
    /// `theirs` is the other side's app version, for the notice that says so.
    Incompatible {
        behind: Behind,
        theirs: String,
    },
    /// Connected, then the network went and did not come back in time.
    ConnectionLost,
    /// The platform would not have the call, as [`Command::Refused`] said.
    Refused,
    Failed(String),
}

enum Control {
    Answer(bool),
    Refused,
    Hangup,
    Media(MediaState),
    AskVideo(bool),
    AnswerVideo(bool),
    /// Someone dialled in to rejoin a call. Only the call can tell whether it is this one.
    Rejoin(Box<Rejoin>),
}

type Signals = mpsc::Receiver<Result<Signal, Error>>;

/// A new connection for a call whose old one dropped, with its signalling open and the other
/// side's hello read: their offer when they re-dialled us, their answer when we re-dialled them.
struct Rejoin {
    connection: Connection,
    send: SendStream,
    signals: Signals,
    theirs: Hello,
}

/// What an outgoing or incoming call task comes to. `None` is a connection that was never a call
/// worth reporting: a re-dial for a call this side no longer has.
type CallOutcome = (Option<EndpointId>, Result<Option<EndReason>, Error>);

pub struct Node {
    commands: mpsc::Sender<Command>,
    engine: JoinHandle<()>,
}

impl Node {
    /// `app` is this build's version as people see it; it goes to the other side of every call, so
    /// that whichever phone is too old can say what it is next to what it should be.
    pub async fn start(secret: SecretKey, network: Network, app: &str) -> Result<(Self, mpsc::Receiver<Event>), Error> {
        let endpoint = bind(secret, &network).await?;
        let (events, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (commands, commands_rx) = mpsc::channel(COMMAND_QUEUE);
        emit(&events, Event::Ready { id: endpoint.id() }).await;
        let (in_call, in_call_rx) = watch::channel(false);
        let (platform_network, platform_network_rx) = watch::channel(true);
        let pilot = match network {
            Network::Public(store) => {
                drop(tokio::spawn(watch_reachable(endpoint.clone(), events.clone(), platform_network_rx)));
                drop(tokio::spawn(heartbeat(endpoint.clone())));
                let (steer, steer_rx) = mpsc::channel(CONTROL_QUEUE);
                let pilot = Pilot::new(endpoint.clone(), store, events.clone());
                drop(tokio::spawn(pilot.run(steer_rx, in_call_rx)));
                Some(steer)
            }
            Network::Local(lookup) => {
                lookup.add_endpoint_info(loopback_addr(&endpoint));
                None
            }
        };
        let (finished, finished_rx) = mpsc::channel(CONTROL_QUEUE);
        let engine = Engine {
            endpoint,
            events,
            call: None,
            next_call: 0,
            finished,
            finished_rx,
            hello: Hello::ours(app),
            pilot,
            in_call,
            platform_network,
        };
        Ok((Self { commands, engine: tokio::spawn(engine.run(commands_rx)) }, events_rx))
    }

    pub async fn send(&self, command: Command) -> Result<(), Error> {
        self.commands.send(command).await.map_err(|_| Error::NodeStopped)
    }

    /// A cloneable, non-owning command sender, e.g. for UI callbacks.
    pub fn handle(&self) -> NodeHandle {
        NodeHandle { commands: self.commands.downgrade() }
    }

    /// Ends any call and closes the endpoint.
    pub async fn shutdown(self) {
        // Handles only hold weak senders: dropping ours closes the channel and stops the engine.
        drop(self.commands);
        if let Err(e) = self.engine.await {
            tracing::error!("node engine: {e}");
        }
    }
}

/// Sends commands without keeping the node alive: only [`Node`] owns its lifetime.
#[derive(Clone, Debug)]
pub struct NodeHandle {
    commands: mpsc::WeakSender<Command>,
}

impl NodeHandle {
    /// Non-blocking, so it works from threads without a runtime (UI callbacks).
    pub fn try_send(&self, command: Command) -> Result<(), Error> {
        let commands = self.commands.upgrade().ok_or(Error::NodeStopped)?;
        commands.try_send(command).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => Error::CommandQueueFull,
            mpsc::error::TrySendError::Closed(_) => Error::NodeStopped,
        })
    }
}

/// Where peers are found.
#[derive(Clone)]
pub enum Network {
    /// The real internet: relays plus DNS/pkarr address lookup. Which relays is a setting, read
    /// and kept current by [`crate::pilot`] from this store.
    Public(Settings),
    /// Loopback only; nodes find each other through a shared in-memory lookup (tests, local demos).
    Local(MemoryLookup),
}

async fn bind(secret: SecretKey, network: &Network) -> Result<Endpoint, Error> {
    let builder = match network {
        // Every relay in the map is handshaked with on every net_report — every 20 to 26 seconds,
        // for the life of the process, call or no call — so the number of them is what the idle
        // cost is made of, and why the pilot keeps it to two.
        //
        // The probes themselves are left at iroh's defaults. Turning the HTTPS latency probe and
        // the captive-portal check off was measured and saved nothing: the beat put the cost at
        // ~13.8 KB per relay per sweep before and ~14.0 KB after, so it is QUIC address discovery
        // that is expensive, not those. They are the only way to find a home relay on a network
        // that blocks QUIC, which is not a trade worth making for noise.
        Network::Public(store) => {
            let map = RelayMap::from_iter(Pilot::initial(store));
            Endpoint::builder(presets::N0).relay_mode(RelayMode::Custom(map))
        }
        Network::Local(lookup) => Endpoint::builder(presets::Minimal).address_lookup(lookup.clone()),
    };
    // iroh's own transport defaults, which its holepunching is tuned for, with the keep-alive
    // shortened (what lets a call tell a stalled network from two quiet people) and room for
    // more video frames at once: one stream each, held until delivered or stale.
    let transport = QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_concurrent_uni_streams(FRAME_STREAMS)
        .build();
    Ok(builder
        .transport_config(transport)
        .crypto_provider(crypto::provider())
        .secret_key(secret)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?)
}

/// The endpoint's bound sockets as loopback addresses.
fn loopback_addr(endpoint: &Endpoint) -> EndpointAddr {
    let addrs = endpoint.bound_sockets().into_iter().map(|socket| {
        let ip = match socket.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        };
        TransportAddr::Ip(SocketAddr::new(ip, socket.port()))
    });
    EndpointAddr::from_parts(endpoint.id(), addrs)
}

async fn emit(events: &mpsc::Sender<Event>, event: Event) {
    if events.send(event).await.is_err() {
        tracing::debug!("event dropped: no listener");
    }
}

/// Reachability, for as long as the endpoint lives. Being connected to a home relay is what makes
/// us dialable, so it is a truer answer than whether the device has an interface up — a phone on
/// a captive-portal wifi has a network and is not reachable. The platform's network only says
/// why not, and [`Reachability`] keeps either from flickering.
async fn watch_reachable(endpoint: Endpoint, events: mpsc::Sender<Event>, mut network: watch::Receiver<bool>) {
    let mut status = endpoint.home_relay_status();
    let mut reach = Reachability::new(Instant::now());
    let mut shown = None;
    loop {
        let now = Instant::now();
        let online = status.get().into_iter().any(|relay| relay.is_connected());
        if online != reach.relay_up() {
            tracing::info!(online, addr = ?endpoint.addr(), "reachability");
            reach.relay(online, now);
        }
        reach.network(*network.borrow_and_update(), now);
        let answer = reach.shown(now);
        if shown != Some(answer) {
            shown = Some(answer);
            tracing::info!(?answer, "reach");
            emit(&events, Event::Reach(answer)).await;
        }
        let deadline = reach.next_change(now);
        tokio::select! {
            updated = status.updated() => if updated.is_err() { break },
            changed = network.changed() => if changed.is_err() { break },
            () = until(deadline) => {}
        }
    }
}

/// A timer that may never fire.
async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// The endpoint's own counters at one instant. Only differences are ever reported: the totals
/// since the process started say nothing about whether it is still working now.
#[derive(Clone, Copy, Default)]
struct Counters {
    relay_up: u64,
    relay_down: u64,
    direct_up: u64,
    direct_down: u64,
    relay_conns: u64,
    relay_fails: u64,
    holepunches: u64,
    reports: u64,
    portmaps: u64,
}

impl Counters {
    fn read(endpoint: &Endpoint) -> Self {
        let metrics = endpoint.metrics();
        let (socket, net) = (&metrics.socket, &metrics.net_report);
        Self {
            relay_up: socket.send_relay.get(),
            relay_down: socket.recv_data_relay.get(),
            direct_up: socket.send_ipv4.get() + socket.send_ipv6.get(),
            direct_down: socket.recv_data_ipv4.get() + socket.recv_data_ipv6.get(),
            relay_conns: socket.relay_conns_success.get(),
            relay_fails: socket.relay_conns_failed.get(),
            holepunches: socket.holepunch_attempts.get(),
            reports: net.reports.get(),
            portmaps: net.portmap_attempts.get(),
        }
    }

    /// Saturating, because a counter can only be reset by a restart, which resets us too.
    const fn since(self, earlier: Self) -> Self {
        Self {
            relay_up: self.relay_up.saturating_sub(earlier.relay_up),
            relay_down: self.relay_down.saturating_sub(earlier.relay_down),
            direct_up: self.direct_up.saturating_sub(earlier.direct_up),
            direct_down: self.direct_down.saturating_sub(earlier.direct_down),
            relay_conns: self.relay_conns.saturating_sub(earlier.relay_conns),
            relay_fails: self.relay_fails.saturating_sub(earlier.relay_fails),
            holepunches: self.holepunches.saturating_sub(earlier.holepunches),
            reports: self.reports.saturating_sub(earlier.reports),
            portmaps: self.portmaps.saturating_sub(earlier.portmaps),
        }
    }
}

/// What the endpoint did between two beats, for as long as it lives.
///
/// A backgrounded app leaves no other trace: the window is gone, nothing else logs, and a night
/// of silence reads the same whether the endpoint was listening or long dead. This says which,
/// and splits the bytes by path, so an idle app that is nonetheless busy on the wire shows up
/// here rather than only as a battery figure the next morning.
///
/// `elapsed` is reported because it is not `BEAT`. The tokio timer counts only time the CPU was
/// awake, so it always waits `BEAT` of that; the wall clock keeps running through suspend, so a
/// beat that took much longer than it asked for is how long the device was actually asleep — the
/// one thing Doze otherwise hides. It was an `Instant`, which stops in suspend too, so it always
/// read 300. A wall clock set backwards reads as 0.
async fn heartbeat(endpoint: Endpoint) {
    let mut last = Counters::read(&endpoint);
    let mut at = SystemTime::now();
    loop {
        tokio::time::sleep(BEAT).await;
        let (now, counters) = (SystemTime::now(), Counters::read(&endpoint));
        let beat = counters.since(last);
        let home = endpoint.home_relay_status().get().into_iter().find(|relay| relay.is_connected());
        tracing::info!(
            elapsed_s = now.duration_since(at).unwrap_or_default().as_secs(),
            relay = home.is_some(),
            home = home.as_ref().map_or_else(String::new, |relay| relay.url().to_string()),
            relay_up = beat.relay_up,
            relay_down = beat.relay_down,
            direct_up = beat.direct_up,
            direct_down = beat.direct_down,
            relay_conns = beat.relay_conns,
            relay_fails = beat.relay_fails,
            holepunches = beat.holepunches,
            reports = beat.reports,
            portmaps = beat.portmaps,
            "beat"
        );
        (last, at) = (counters, now);
    }
}

struct ActiveCall {
    id: u64,
    control: mpsc::Sender<Control>,
}

struct Engine {
    endpoint: Endpoint,
    events: mpsc::Sender<Event>,
    call: Option<ActiveCall>,
    next_call: u64,
    finished: mpsc::Sender<u64>,
    finished_rx: mpsc::Receiver<u64>,
    /// What this build says about itself in every offer and answer.
    hello: Hello,
    /// The relay pilot's commands; none on a local network.
    pilot: Option<mpsc::Sender<Steer>>,
    /// Whether a call is up, which the pilot waits on before it moves the home relay.
    in_call: watch::Sender<bool>,
    /// Whether the platform has a network, for the reachability answer.
    platform_network: watch::Sender<bool>,
}

impl Engine {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                incoming = self.endpoint.accept() => match incoming {
                    Some(incoming) => self.incoming(incoming),
                    None => break,
                },
                Some(id) = self.finished_rx.recv() => {
                    if self.call.as_ref().is_some_and(|call| call.id == id) {
                        self.call = None;
                        self.in_call.send_replace(false);
                    }
                }
            }
        }
        if let Some(call) = self.call.take() {
            forward(&call, Control::Hangup).await;
            if tokio::time::timeout(SHUTDOWN_GRACE, self.finished_rx.recv()).await.is_err() {
                tracing::debug!("call did not finish before shutdown");
            }
        }
        self.endpoint.close().await;
        tracing::info!("node stopped");
    }

    async fn command(&mut self, command: Command) {
        match (command, &self.call) {
            (Command::Relays(steer), _) => match &self.pilot {
                Some(pilot) => {
                    if pilot.send(steer).await.is_err() {
                        tracing::warn!(?steer, "relay pilot gone");
                    }
                }
                None => tracing::debug!(?steer, "no relays on a local network"),
            },
            (Command::Network(up), _) => {
                self.platform_network.send_if_modified(|was| std::mem::replace(was, up) != up);
                emit(&self.events, Event::Network(up)).await;
                // Only a network that is there has anything to re-read. Told while there is none,
                // iroh keeps just the public fallbacks and then misses the next one arriving.
                if up {
                    self.endpoint.network_change().await;
                }
            }
            (Command::Call(peer, mode), None) => {
                let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
                let task =
                    outgoing(self.endpoint.clone(), peer, mode, control_rx, self.events.clone(), self.hello.clone());
                self.spawn_call(control, Some(peer), task);
            }
            // Refused, and said only in the log. It used to go out as `Ended`, which every listener
            // read as the end of the call that *is* up: the app logged that call as failed and
            // tore its media down while it carried on.
            (Command::Call(peer, _), Some(_)) => {
                tracing::warn!(peer = %peer.fmt_short(), "refused a call while one is up");
            }
            (Command::Answer(accept), Some(call)) => forward(call, Control::Answer(accept)).await,
            (Command::Refused, Some(call)) => forward(call, Control::Refused).await,
            (Command::Hangup, Some(call)) => forward(call, Control::Hangup).await,
            (Command::Media(state), Some(call)) => forward(call, Control::Media(state)).await,
            (Command::AskVideo(ask), Some(call)) => forward(call, Control::AskVideo(ask)).await,
            (Command::AnswerVideo(accept), Some(call)) => forward(call, Control::AnswerVideo(accept)).await,
            (_, None) => tracing::debug!(?command, "no call"),
        }
    }

    fn incoming(&mut self, incoming: Incoming) {
        if let Some(call) = &self.call {
            tokio::spawn(screen(incoming, call.control.clone()));
            return;
        }
        let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let task = answer(incoming, self.endpoint.clone(), control_rx, self.events.clone(), self.hello.clone());
        self.spawn_call(control, None, task);
    }

    /// Runs a call task; it reports its own end, then frees the call slot.
    fn spawn_call(
        &mut self,
        control: mpsc::Sender<Control>,
        peer: Option<EndpointId>,
        task: impl Future<Output = CallOutcome> + Send + 'static,
    ) {
        let id = self.next_call;
        self.next_call += 1;
        self.call = Some(ActiveCall { id, control });
        self.in_call.send_replace(true);
        let (events, finished) = (self.events.clone(), self.finished.clone());
        tokio::spawn(async move {
            let (known_peer, outcome) = task.await;
            match outcome.unwrap_or_else(|e| Some(EndReason::Failed(e.to_string()))) {
                Some(reason) => {
                    tracing::info!(peer = ?known_peer.or(peer), ?reason, "call ended");
                    emit(&events, Event::Ended { peer: known_peer.or(peer), reason }).await;
                }
                None => tracing::info!(peer = ?known_peer.or(peer), "turned away a re-dial for a call that is over"),
            }
            if finished.send(id).await.is_err() {
                tracing::debug!("engine gone before call finished");
            }
        });
    }
}

async fn forward(call: &ActiveCall, control: Control) {
    if call.control.send(control).await.is_err() {
        tracing::debug!("call task already finished");
    }
}

/// Reads signals into a channel until the stream fails or ends.
fn spawn_reader(mut recv: iroh::endpoint::RecvStream) -> mpsc::Receiver<Result<Signal, Error>> {
    let (tx, rx) = mpsc::channel(SIGNAL_QUEUE);
    tokio::spawn(async move {
        loop {
            let signal = protocol::recv(&mut recv).await;
            let failed = signal.is_err();
            if tx.send(signal).await.is_err() || failed {
                break;
            }
        }
    });
    rx
}

/// Sends a final signal and gives the peer time to close before closing ourselves.
async fn finish(
    connection: &Connection,
    send: &mut SendStream,
    signal: Signal,
    code: iroh::endpoint::VarInt,
) -> Result<(), Error> {
    protocol::send(send, signal).await?;
    if tokio::time::timeout(CLOSE_GRACE, connection.closed()).await.is_err() {
        connection.close(code, b"");
    }
    Ok(())
}

async fn outgoing(
    endpoint: Endpoint,
    peer: EndpointId,
    mode: Mode,
    control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
    hello: Hello,
) -> CallOutcome {
    (Some(peer), dial(endpoint, peer, mode, control, events, hello).await.map(Some))
}

/// A name for a new call, unique enough that a re-dial can never be taken for a different call
/// with the same person. `RandomState` is std's own randomly keyed hasher: no extra dependency.
fn new_call_id() -> u64 {
    RandomState::new().hash_one(SystemTime::now())
}

/// Turns away a connection that came to rejoin a call this side cannot give it.
async fn refuse(mut rejoin: Rejoin, signal: Signal, code: iroh::endpoint::VarInt) {
    tracing::info!(peer = %rejoin.connection.remote_id().fmt_short(), ?signal, "refused a re-dial");
    if let Err(e) = finish(&rejoin.connection, &mut rejoin.send, signal, code).await {
        tracing::debug!("refusing a re-dial: {e}");
    }
}

/// Ends a call the two sides cannot have, telling the other why, and says which side is behind.
async fn incompatible(
    connection: &Connection,
    send: &mut SendStream,
    ours: &Hello,
    theirs: &Hello,
    behind: Behind,
) -> Result<EndReason, Error> {
    tracing::warn!(?behind, ours = ours.app, theirs = theirs.app, "cannot call: one side needs an update");
    finish(connection, send, Signal::Incompatible(ours.clone()), CLOSE_INCOMPATIBLE).await?;
    Ok(EndReason::Incompatible { behind, theirs: theirs.app.clone() })
}

async fn dial(
    endpoint: Endpoint,
    peer: EndpointId,
    mode: Mode,
    mut control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
    hello: Hello,
) -> Result<EndReason, Error> {
    emit(&events, Event::Dialing { peer, mode }).await;
    // What the user does to the mic before they answer is theirs to hear about once they do.
    let mut ours = MediaState::default();
    // Dialling a key nobody is listening on has no natural end: iroh keeps trying relays and
    // holepunching for as long as it is asked to, so the deadline has to come from here.
    let dialling = tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(peer, ALPN));
    tokio::pin!(dialling);
    let connection = loop {
        tokio::select! {
            connection = &mut dialling => match connection {
                Ok(connection) => break connection?,
                Err(_) => return Ok(EndReason::DialTimeout),
            },
            command = control.recv() => match command {
                Some(Control::Hangup) | None => return Ok(EndReason::LocalHangup),
                Some(Control::Refused) => return Ok(EndReason::Refused),
                Some(Control::Media(state)) => ours = state,
                Some(Control::Rejoin(rejoin)) => refuse(*rejoin, Signal::Busy, CLOSE_BUSY).await,
                Some(Control::Answer(_) | Control::AskVideo(_) | Control::AnswerVideo(_)) => {}
            },
        }
    };
    let key_exchange = secure(&connection)?;
    let (mut send, recv) = connection.open_bi().await?;
    let setup = Setup { call: new_call_id(), voice: mode == Mode::Voice, resume: false };
    protocol::send(&mut send, Signal::Offer(hello.offer(setup))).await?;
    let mut signals = spawn_reader(recv);
    emit(&events, Event::Ringing { peer }).await;
    // Reached them, so now it is a question of whether anyone picks up. Hanging up properly on
    // the way out stops their phone ringing too.
    let unanswered = tokio::time::sleep(RING_TIMEOUT);
    tokio::pin!(unanswered);
    loop {
        tokio::select! {
            () = &mut unanswered => {
                finish(&connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
                return Ok(EndReason::NoAnswer);
            }
            signal = signals.recv() => return match signal {
                // Their answer carries what they need; the check is ours to make, since only we
                // know whether we can do it.
                Some(Ok(Signal::Accept(theirs))) => match protocol::behind(&hello, &theirs) {
                    Some(behind) => incompatible(&connection, &mut send, &hello, &theirs, behind).await,
                    None => {
                        let call = Call { peer, key_exchange, mode, setup, ours };
                        let link = Rejoin { connection, send, signals, theirs };
                        connected(link, call, &endpoint, &mut control, &events, &hello).await
                    }
                },
                // They could not take our offer. Which of us is behind is in the two hellos.
                Some(Ok(Signal::Incompatible(theirs))) => {
                    let behind = protocol::behind(&hello, &theirs).unwrap_or(Behind::Them);
                    Ok(EndReason::Incompatible { behind, theirs: theirs.app })
                }
                Some(Ok(Signal::Reject)) => Ok(EndReason::Rejected),
                Some(Ok(Signal::Busy)) => Ok(EndReason::Busy),
                Some(Ok(Signal::Hangup)) | None => Ok(EndReason::RemoteHangup),
                Some(Ok(Signal::Unknown)) => {
                    tracing::debug!("ignored a signal from a newer build");
                    continue;
                }
                // Nothing to say about a call that has not started.
                Some(Ok(Signal::Media(_) | Signal::AskVideo | Signal::WithdrawVideo | Signal::AnswerVideo(_))) => {
                    continue;
                }
                Some(Ok(Signal::Offer(_) | Signal::KeyframeRequest)) => {
                    protocol_error(&connection, "unexpected signal while ringing")
                }
                Some(Err(e)) => Err(e),
            },
            command = control.recv() => match command {
                Some(Control::Hangup) | None => {
                    finish(&connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
                    return Ok(EndReason::LocalHangup);
                }
                Some(Control::Refused) => {
                    finish(&connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
                    return Ok(EndReason::Refused);
                }
                Some(Control::Media(state)) => ours = state,
                Some(Control::Rejoin(rejoin)) => refuse(*rejoin, Signal::Busy, CLOSE_BUSY).await,
                Some(Control::Answer(_) | Control::AskVideo(_) | Control::AnswerVideo(_)) => {
                    tracing::debug!("ignored on a call not yet answered");
                }
            },
        }
    }
}

async fn answer(
    incoming: Incoming,
    endpoint: Endpoint,
    mut control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
    hello: Hello,
) -> CallOutcome {
    let connection = match accept_connection(incoming).await {
        Ok(connection) => connection,
        Err(e) => return (None, Err(e)),
    };
    let peer = connection.remote_id();
    (Some(peer), ring(connection, &endpoint, &mut control, events, &hello).await)
}

async fn accept_connection(incoming: Incoming) -> Result<Connection, Error> {
    Ok(incoming.accept()?.await?)
}

/// Reads up to the offer that opens a call's signalling.
async fn first_offer(connection: &Connection, signals: &mut Signals) -> Result<Hello, Error> {
    loop {
        match signals.recv().await {
            Some(Ok(Signal::Offer(theirs))) => return Ok(theirs),
            Some(Ok(Signal::Unknown)) => tracing::debug!("ignored a signal from a newer build"),
            Some(Err(e)) => return Err(e),
            Some(Ok(_)) | None => return protocol_error(connection, "expected offer").map(|_| Hello::default()),
        }
    }
}

async fn ring(
    connection: Connection,
    endpoint: &Endpoint,
    control: &mut mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
    hello: &Hello,
) -> Result<Option<EndReason>, Error> {
    let peer = connection.remote_id();
    let key_exchange = secure(&connection)?;
    let (mut send, recv) = connection.accept_bi().await?;
    let mut signals = spawn_reader(recv);
    let theirs = first_offer(&connection, &mut signals).await?;
    // Checked before anything rings: a call that cannot happen should not ring, only say why.
    if let Some(behind) = protocol::behind(hello, &theirs) {
        return incompatible(&connection, &mut send, hello, &theirs, behind).await.map(Some);
    }
    let setup = theirs.setup.unwrap_or_default();
    // A re-dial for a call that has already ended here: nothing to ring for, and nothing to log.
    if setup.resume {
        finish(&connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
        return Ok(None);
    }
    let mode = if setup.voice { Mode::Voice } else { Mode::Video };
    tracing::info!(%peer, protocol = theirs.protocol, app = theirs.app, ?mode, "incoming call");
    emit(&events, Event::Incoming { peer, mode }).await;
    let mut ours = MediaState::default();
    loop {
        tokio::select! {
            signal = signals.recv() => return match signal {
                Some(Ok(Signal::Hangup)) | None => Ok(Some(EndReason::RemoteHangup)),
                Some(Ok(Signal::Unknown | Signal::Media(_))) => continue,
                Some(Ok(_)) => protocol_error(&connection, "unexpected signal while ringing").map(Some),
                Some(Err(e)) => Err(e),
            },
            command = control.recv() => match command {
                Some(Control::Answer(true)) => {
                    protocol::send(&mut send, Signal::Accept(hello.clone())).await?;
                    let call = Call { peer, key_exchange, mode, setup, ours };
                    let link = Rejoin { connection, send, signals, theirs };
                    return connected(link, call, endpoint, control, &events, hello).await.map(Some);
                }
                Some(Control::Answer(false) | Control::Hangup) | None => {
                    finish(&connection, &mut send, Signal::Reject, CLOSE_REJECTED).await?;
                    return Ok(Some(EndReason::Declined));
                }
                Some(Control::Refused) => {
                    finish(&connection, &mut send, Signal::Busy, CLOSE_BUSY).await?;
                    return Ok(Some(EndReason::Refused));
                }
                Some(Control::Media(state)) => ours = state,
                Some(Control::Rejoin(rejoin)) => refuse(*rejoin, Signal::Busy, CLOSE_BUSY).await,
                Some(Control::AskVideo(_) | Control::AnswerVideo(_)) => {
                    tracing::debug!("ignored on a call not yet answered");
                }
            },
        }
    }
}

/// What a call was set up as, carried from ringing into the call.
struct Call {
    peer: EndpointId,
    key_exchange: NamedGroup,
    mode: Mode,
    setup: Setup,
    ours: MediaState,
}

/// Starts the media and runs the call until it ends, over as many connections as it takes.
async fn connected(
    link: Rejoin,
    call: Call,
    endpoint: &Endpoint,
    control: &mut mpsc::Receiver<Control>,
    events: &mpsc::Sender<Event>,
    hello: &Hello,
) -> Result<EndReason, Error> {
    let Call { peer, key_exchange, mode, setup, ours } = call;
    tracing::info!(%peer, ?key_exchange, ?mode, "call connected");
    let (media, links) = media::start(&link.connection, endpoint)?;
    emit(events, Event::Connected { peer, key_exchange, mode, media: Box::new(media) }).await;
    let mut live = Live {
        endpoint,
        events,
        control,
        hello,
        peer,
        call: setup.call,
        link,
        links,
        video: mode == Mode::Video,
        ours,
        asked: false,
        their_ask: false,
    };
    live.retell().await;
    live.run().await
}

/// How a stretch of a call on one connection ended.
enum Turn {
    Over(EndReason),
    Lost,
}

/// A connected call, across every connection it comes to have.
struct Live<'a> {
    endpoint: &'a Endpoint,
    events: &'a mpsc::Sender<Event>,
    control: &'a mut mpsc::Receiver<Control>,
    hello: &'a Hello,
    peer: EndpointId,
    /// The name its offer gave it, which a re-dial must repeat.
    call: u64,
    /// The connection it runs over now, with its signalling and the other side's hello.
    link: Rejoin,
    links: MediaLinks,
    video: bool,
    /// What we last said about our mic and camera, said again after a rejoin.
    ours: MediaState,
    /// We asked to switch to video and have no answer yet.
    asked: bool,
    /// They asked, and we have not answered.
    their_ask: bool,
}

impl Live<'_> {
    async fn run(mut self) -> Result<EndReason, Error> {
        loop {
            match self.connected().await? {
                Turn::Over(reason) => return Ok(reason),
                Turn::Lost => {
                    MediaStats::count(&self.links.stats.drops, 1);
                    tracing::info!("connection lost; trying to rejoin");
                    emit(self.events, Event::Reconnecting).await;
                    if let Some(reason) = self.rejoin().await {
                        return Ok(reason);
                    }
                    emit(self.events, Event::Reconnected).await;
                    self.retell().await;
                }
            }
        }
    }

    /// Runs the call over its current connection until it ends or the connection goes.
    async fn connected(&mut self) -> Result<Turn, Error> {
        loop {
            tokio::select! {
                signal = self.link.signals.recv() => match signal {
                    Some(Ok(signal)) => {
                        if let Some(turn) = self.signal(signal).await? {
                            return Ok(turn);
                        }
                    }
                    Some(Err(e)) => return self.stopped(e),
                    None => return self.stopped(Error::Protocol("signalling ended")),
                },
                Some(()) = self.links.request_keyframe.recv() => self.say(Signal::KeyframeRequest).await,
                command = self.control.recv() => {
                    if let Some(turn) = self.command(command).await {
                        return Ok(turn);
                    }
                }
            }
        }
    }

    /// Why signalling stopped: the network, or their hang-up arriving without its signal, or a
    /// failure that is neither.
    fn stopped(&self, e: Error) -> Result<Turn, Error> {
        match self.link.connection.close_reason() {
            Some(ConnectionError::TimedOut | ConnectionError::Reset) => Ok(Turn::Lost),
            Some(ConnectionError::ApplicationClosed(close)) if close.error_code == CLOSE_HANGUP => {
                Ok(Turn::Over(EndReason::RemoteHangup))
            }
            _ => Err(e),
        }
    }

    async fn signal(&mut self, signal: Signal) -> Result<Option<Turn>, Error> {
        match signal {
            Signal::KeyframeRequest => {
                self.links.stats.keyframe_requests_received.fetch_add(1, Ordering::Relaxed);
                if self.links.keyframe_requested.try_send(()).is_err() {
                    tracing::debug!("keyframe request already pending");
                }
            }
            Signal::Hangup => {
                self.link.connection.close(CLOSE_HANGUP, b"");
                return Ok(Some(Turn::Over(EndReason::RemoteHangup)));
            }
            Signal::Media(state) => emit(self.events, Event::PeerMedia(state)).await,
            Signal::AskVideo if self.video => tracing::debug!("asked for video on a video call"),
            // Both asked at once: each is the other's yes.
            Signal::AskVideo if self.asked => {
                self.asked = false;
                self.their_ask = true;
                self.answer_video(true).await;
            }
            Signal::AskVideo => {
                self.their_ask = true;
                emit(self.events, Event::VideoAsked(true)).await;
            }
            Signal::WithdrawVideo => {
                if std::mem::take(&mut self.their_ask) {
                    emit(self.events, Event::VideoAsked(false)).await;
                }
            }
            Signal::AnswerVideo(accepted) => {
                if std::mem::take(&mut self.asked) {
                    self.video |= accepted;
                    emit(self.events, if accepted { Event::VideoOn } else { Event::VideoDeclined }).await;
                }
            }
            Signal::Unknown => tracing::debug!("ignored a signal from a newer build"),
            Signal::Offer(_) | Signal::Accept(_) | Signal::Reject | Signal::Busy | Signal::Incompatible(_) => {
                return protocol_error(&self.link.connection, "unexpected signal in call").map(|_| None);
            }
        }
        Ok(None)
    }

    /// Said if it can be; a hang-up is a hang-up whether or not the network carries it.
    async fn hang_up(&mut self, reason: EndReason) -> Turn {
        if let Err(e) = finish(&self.link.connection, &mut self.link.send, Signal::Hangup, CLOSE_HANGUP).await {
            tracing::debug!("hanging up: {e}");
        }
        Turn::Over(reason)
    }

    async fn command(&mut self, command: Option<Control>) -> Option<Turn> {
        match command {
            Some(Control::Hangup) | None => return Some(self.hang_up(EndReason::LocalHangup).await),
            Some(Control::Refused) => return Some(self.hang_up(EndReason::Refused).await),
            Some(Control::Answer(_)) => tracing::debug!("answer ignored during call"),
            Some(Control::Media(state)) => {
                self.ours = state;
                self.say(Signal::Media(state)).await;
            }
            // Asking back is saying yes.
            Some(Control::AskVideo(true)) if self.their_ask => self.answer_video(true).await,
            Some(Control::AskVideo(true)) => {
                if self.video || self.asked {
                    tracing::debug!(video = self.video, asked = self.asked, "not asking for video");
                } else {
                    self.asked = true;
                    self.say(Signal::AskVideo).await;
                }
            }
            Some(Control::AskVideo(false)) => {
                if std::mem::take(&mut self.asked) {
                    self.say(Signal::WithdrawVideo).await;
                }
            }
            Some(Control::AnswerVideo(accepted)) => self.answer_video(accepted).await,
            // They lost the connection before we noticed, and are back on a new one.
            Some(Control::Rejoin(rejoin)) => {
                if self.accept_rejoin(*rejoin).await {
                    MediaStats::count(&self.links.stats.drops, 1);
                    self.retell().await;
                }
            }
        }
        None
    }

    async fn answer_video(&mut self, accepted: bool) {
        if !std::mem::take(&mut self.their_ask) {
            return;
        }
        self.say(Signal::AnswerVideo(accepted)).await;
        if accepted {
            self.video = true;
            emit(self.events, Event::VideoOn).await;
        }
    }

    /// A signal that matters only while the call is up. A lost connection is for the reader to
    /// notice, which it will; failing here as well would say the same thing twice.
    async fn say(&mut self, signal: Signal) {
        if let Err(e) = protocol::send(&mut self.link.send, signal).await {
            tracing::debug!("signal not sent: {e}");
        }
    }

    /// What they may have missed while the connection was down.
    async fn retell(&mut self) {
        if self.ours != MediaState::default() {
            self.say(Signal::Media(self.ours)).await;
        }
        if self.asked {
            self.say(Signal::AskVideo).await;
        }
    }

    /// Gets the call back onto a new connection within [`RESUME_GRACE`]; the reason it ended
    /// otherwise. Only the side with the lower key re-dials, so the two never dial each other at
    /// once; the other waits for it.
    async fn rejoin(&mut self) -> Option<EndReason> {
        let grace = tokio::time::sleep(RESUME_GRACE);
        tokio::pin!(grace);
        let redials = self.endpoint.id() < self.peer;
        let offer = self.hello.offer(Setup { call: self.call, voice: false, resume: true });
        let (endpoint, peer) = (self.endpoint, self.peer);
        let mut pause = false;
        loop {
            let attempt = async {
                if !redials {
                    return std::future::pending().await;
                }
                if pause {
                    tokio::time::sleep(REDIAL_PAUSE).await;
                }
                redial(endpoint, peer, &offer).await
            };
            tokio::select! {
                () = &mut grace => {
                    tracing::info!("could not rejoin in time");
                    return Some(EndReason::ConnectionLost);
                }
                attempt = attempt => match attempt {
                    Ok(Some(link)) => {
                        self.adopt(link);
                        return None;
                    }
                    Ok(None) => return Some(EndReason::ConnectionLost),
                    Err(e) => {
                        tracing::info!("re-dial failed: {e}");
                        pause = true;
                    }
                },
                command = self.control.recv() => match command {
                    // Nothing to tell them over; their own grace runs out instead.
                    Some(Control::Hangup) | None => return Some(EndReason::LocalHangup),
                    Some(Control::Refused) => return Some(EndReason::Refused),
                    Some(Control::Media(state)) => self.ours = state,
                    Some(Control::Rejoin(rejoin)) => {
                        if self.accept_rejoin(*rejoin).await {
                            return None;
                        }
                    }
                    Some(Control::Answer(_) | Control::AskVideo(_) | Control::AnswerVideo(_)) => {
                        tracing::debug!("ignored while reconnecting");
                    }
                },
            }
        }
    }

    /// Takes a re-dial the other side made, if it is this call; turns it away otherwise.
    async fn accept_rejoin(&mut self, mut rejoin: Rejoin) -> bool {
        let named = rejoin.theirs.setup.map(|setup| setup.call);
        if rejoin.connection.remote_id() != self.peer || named != Some(self.call) {
            refuse(rejoin, Signal::Busy, CLOSE_BUSY).await;
            return false;
        }
        if let Err(e) = protocol::send(&mut rejoin.send, Signal::Accept(self.hello.clone())).await {
            tracing::info!("answering a re-dial: {e}");
            return false;
        }
        self.adopt(rejoin);
        true
    }

    /// Carries the call on over a new connection, and lets the old one go.
    fn adopt(&mut self, link: Rejoin) {
        let old = std::mem::replace(&mut self.link, link);
        old.connection.close(CLOSE_REJOINED, b"");
        self.links.rejoin(&self.link.connection);
        tracing::info!(peer = %self.peer.fmt_short(), "call rejoined");
    }
}

/// Dials a dropped call's peer again with an offer that names it. `None` when they answer with
/// anything but taking it back: the call is over on their side.
async fn redial(endpoint: &Endpoint, peer: EndpointId, offer: &Hello) -> Result<Option<Rejoin>, Error> {
    let connection = endpoint.connect(peer, ALPN).await?;
    secure(&connection)?;
    let (mut send, recv) = connection.open_bi().await?;
    protocol::send(&mut send, Signal::Offer(offer.clone())).await?;
    let mut signals = spawn_reader(recv);
    loop {
        match signals.recv().await {
            Some(Ok(Signal::Accept(theirs))) => return Ok(Some(Rejoin { connection, send, signals, theirs })),
            Some(Ok(Signal::Unknown)) => {}
            Some(Ok(answer)) => {
                tracing::info!(?answer, "their phone has no call to rejoin");
                return Ok(None);
            }
            Some(Err(e)) => return Err(e),
            None => return Err(Error::Protocol("no answer to a re-dial")),
        }
    }
}

/// Refuses peer connections that did not negotiate a post-quantum key exchange.
fn secure(connection: &Connection) -> Result<NamedGroup, Error> {
    crypto::require_post_quantum(connection).inspect_err(|e| {
        tracing::warn!(peer = %connection.remote_id(), "refusing connection: {e}");
        connection.close(CLOSE_NOT_POST_QUANTUM, b"post-quantum key exchange required");
    })
}

fn protocol_error(connection: &Connection, what: &'static str) -> Result<EndReason, Error> {
    connection.close(CLOSE_PROTOCOL, what.as_bytes());
    Err(Error::Protocol(what))
}

/// A connection arriving while a call is up: the other side of that call re-dialling after a
/// drop goes to the call, which alone can tell whether it is the same one; anyone else is busy.
async fn screen(incoming: Incoming, call: mpsc::Sender<Control>) {
    let outcome = async {
        let connection = accept_connection(incoming).await?;
        secure(&connection)?;
        let (mut send, recv) = connection.accept_bi().await?;
        let mut signals = spawn_reader(recv);
        let offer = tokio::time::timeout(OFFER_WAIT, first_offer(&connection, &mut signals)).await;
        let theirs = match offer {
            Ok(Ok(theirs)) if theirs.setup.is_some_and(|setup| setup.resume) => theirs,
            _ => {
                tracing::info!(peer = %connection.remote_id(), "busy: declining call");
                return finish(&connection, &mut send, Signal::Busy, CLOSE_BUSY).await;
            }
        };
        let rejoin = Box::new(Rejoin { connection, send, signals, theirs });
        // The call ended in the meantime: it has nothing to rejoin.
        if let Err(mpsc::error::SendError(Control::Rejoin(rejoin))) = call.send(Control::Rejoin(rejoin)).await {
            refuse(*rejoin, Signal::Hangup, CLOSE_HANGUP).await;
        }
        Ok::<_, Error>(())
    };
    if let Err(e) = outcome.await {
        tracing::debug!("screening a connection mid-call: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::{Context, bail};
    use iroh::endpoint::ConnectionError;
    use rustls::crypto::{CryptoProvider, aws_lc_rs};

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test(flavor = "multi_thread")]
    async fn classical_key_exchange_is_refused() -> anyhow::Result<()> {
        let lookup = MemoryLookup::new();
        let (node, mut events) = Node::start(SecretKey::generate(), Network::Local(lookup.clone()), "test").await?;
        let Some(Event::Ready { id }) = events.recv().await else { bail!("node not ready") };

        let classical =
            Arc::new(CryptoProvider { kx_groups: vec![aws_lc_rs::kx_group::X25519], ..aws_lc_rs::default_provider() });
        let client =
            Endpoint::builder(presets::Minimal).crypto_provider(classical).address_lookup(lookup).bind().await?;
        let connection =
            tokio::time::timeout(TIMEOUT, client.connect(id, ALPN)).await.context("connect timed out")??;
        let closed = tokio::time::timeout(TIMEOUT, connection.closed()).await.context("not closed")?;
        assert!(
            matches!(&closed, ConnectionError::ApplicationClosed(close) if close.error_code == CLOSE_NOT_POST_QUANTUM),
            "unexpected close: {closed:?}"
        );

        client.close().await;
        node.shutdown().await;
        Ok(())
    }

    /// A peer speaking the wire by hand, for what two nodes on loopback cannot be made to do:
    /// the network never drops there, so a re-dial has to be staged.
    async fn by_hand(lookup: MemoryLookup) -> anyhow::Result<(Node, mpsc::Receiver<Event>, EndpointId, Endpoint)> {
        let (node, mut events) = Node::start(SecretKey::generate(), Network::Local(lookup.clone()), "test").await?;
        let Some(Event::Ready { id }) = events.recv().await else { bail!("node not ready") };
        let client = Endpoint::builder(presets::Minimal)
            .crypto_provider(crypto::provider())
            .address_lookup(lookup)
            .bind()
            .await?;
        Ok((node, events, id, client))
    }

    async fn next(events: &mut mpsc::Receiver<Event>) -> anyhow::Result<Event> {
        tokio::time::timeout(TIMEOUT, events.recv()).await.context("no event")?.context("events closed")
    }

    async fn offer(
        client: &Endpoint,
        id: EndpointId,
        setup: Setup,
    ) -> anyhow::Result<(Connection, SendStream, iroh::endpoint::RecvStream)> {
        let connection =
            tokio::time::timeout(TIMEOUT, client.connect(id, ALPN)).await.context("connect timed out")??;
        let (mut send, recv) = connection.open_bi().await?;
        protocol::send(&mut send, Signal::Offer(Hello::ours("by hand").offer(setup))).await?;
        Ok((connection, send, recv))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redial_mid_call_carries_the_call_over() -> anyhow::Result<()> {
        let (node, mut events, id, client) = by_hand(MemoryLookup::new()).await?;
        let call = Setup { call: 42, voice: true, resume: false };
        let (first, _send, mut recv) = offer(&client, id, call).await?;
        assert!(matches!(next(&mut events).await?, Event::Incoming { mode: Mode::Voice, .. }));
        node.send(Command::Answer(true)).await?;
        assert!(matches!(protocol::recv(&mut recv).await?, Signal::Accept(_)));
        assert!(matches!(next(&mut events).await?, Event::Connected { mode: Mode::Voice, .. }));

        // Someone else's call, or an old one: busy, and the call carries on.
        let (_other, _send, mut refused) = offer(&client, id, Setup { call: 7, resume: true, ..call }).await?;
        assert!(matches!(protocol::recv(&mut refused).await?, Signal::Busy));

        let (_second, mut send, mut recv) = offer(&client, id, Setup { resume: true, ..call }).await?;
        assert!(matches!(protocol::recv(&mut recv).await?, Signal::Accept(_)));
        let closed = tokio::time::timeout(TIMEOUT, first.closed()).await.context("old connection kept")?;
        assert!(
            matches!(&closed, ConnectionError::ApplicationClosed(close) if close.error_code == CLOSE_REJOINED),
            "unexpected close: {closed:?}"
        );

        // The same call, over the new connection, until it is hung up there.
        protocol::send(&mut send, Signal::Hangup).await?;
        loop {
            match next(&mut events).await? {
                Event::Ended { reason, .. } => {
                    assert!(matches!(reason, EndReason::RemoteHangup), "ended as {reason:?}");
                    break;
                }
                Event::Incoming { .. } | Event::Connected { .. } => bail!("the re-dial rang as a new call"),
                _ => {}
            }
        }
        client.close().await;
        node.shutdown().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_redial_for_a_call_that_is_over_is_turned_away_quietly() -> anyhow::Result<()> {
        const QUIET: Duration = Duration::from_millis(500);
        let (node, mut events, id, client) = by_hand(MemoryLookup::new()).await?;
        let (_connection, _send, mut recv) = offer(&client, id, Setup { call: 9, voice: false, resume: true }).await?;
        assert!(matches!(protocol::recv(&mut recv).await?, Signal::Hangup));
        assert!(tokio::time::timeout(QUIET, events.recv()).await.is_err(), "a re-dial rang or was logged");
        client.close().await;
        node.shutdown().await;
        Ok(())
    }
}
