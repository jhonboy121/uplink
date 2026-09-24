//! Call engine: owns the iroh endpoint and handles one call at a time.
//!
//! [`Node::start`] binds the endpoint and returns the node (commands in) plus an event receiver.
//! Each call runs in its own task; signals are read by a separate reader task so `select!`
//! never cancels a half-read frame.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::Duration;

use iroh::endpoint::{Connection, Incoming, SendStream, presets};
use iroh::address_lookup::MemoryLookup;
use iroh::{Endpoint, EndpointAddr, RelayMode, SecretKey, TransportAddr, Watcher as _};
use rustls::NamedGroup;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::protocol::{
    self, ALPN, CLOSE_BUSY, CLOSE_HANGUP, CLOSE_NOT_POST_QUANTUM, CLOSE_PROTOCOL, CLOSE_REJECTED, Signal,
};
use crate::media::{self, MediaSession};
use crate::relays::Relays;
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

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Call(EndpointId),
    Answer(bool),
    Hangup,
}

#[derive(Debug)]
pub enum Event {
    Ready { id: EndpointId },
    /// Connected to a home relay, so other peers can reach us. Emitted again as `Offline` when
    /// that stops being true, which is what the reachability chip reads.
    Online,
    Offline,
    Dialing { peer: EndpointId },
    /// Our offer reached the peer; waiting for them to answer.
    Ringing { peer: EndpointId },
    Incoming { peer: EndpointId },
    /// Always post-quantum: other key exchanges are refused.
    Connected { peer: EndpointId, key_exchange: NamedGroup, media: Box<MediaSession> },
    Ended { peer: Option<EndpointId>, reason: EndReason },
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
    Failed(String),
}

enum Control {
    Answer(bool),
    Hangup,
}

pub struct Node {
    commands: mpsc::Sender<Command>,
    engine: JoinHandle<()>,
}

impl Node {
    pub async fn start(secret: SecretKey, network: Network) -> Result<(Self, mpsc::Receiver<Event>), Error> {
        let endpoint = bind(secret, &network).await?;
        let (events, events_rx) = mpsc::channel(EVENT_QUEUE);
        let (commands, commands_rx) = mpsc::channel(COMMAND_QUEUE);
        emit(&events, Event::Ready { id: endpoint.id() }).await;
        match network {
            Network::Public(_) => {
                drop(tokio::spawn(watch_reachable(endpoint.clone(), events.clone())));
                drop(tokio::spawn(heartbeat(endpoint.clone())));
            }
            Network::Local(lookup) => lookup.add_endpoint_info(loopback_addr(&endpoint)),
        }
        let (finished, finished_rx) = mpsc::channel(CONTROL_QUEUE);
        let engine = Engine { endpoint, events, call: None, next_call: 0, finished, finished_rx };
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
#[derive(Clone, Debug)]
pub enum Network {
    /// The real internet: relays plus DNS/pkarr address lookup. Which relays is a setting, so it
    /// is carried rather than assumed — see [`crate::relays`].
    Public(Relays),
    /// Loopback only; nodes find each other through a shared in-memory lookup (tests, local demos).
    Local(MemoryLookup),
}

async fn bind(secret: SecretKey, network: &Network) -> Result<Endpoint, Error> {
    let builder = match network {
        // Every relay in the map is handshaked with on every net_report — every 20 to 26 seconds,
        // for the life of the process, call or no call — so the number of them is what the idle
        // cost is made of, and that is a setting rather than a constant.
        //
        // The probes themselves are left at iroh's defaults. Turning the HTTPS latency probe and
        // the captive-portal check off was measured and saved nothing: the beat put the cost at
        // ~13.8 KB per relay per sweep before and ~14.0 KB after, so it is QUIC address discovery
        // that is expensive, not those. They are the only way to find a home relay on a network
        // that blocks QUIC, which is not a trade worth making for noise.
        Network::Public(relays) => {
            Endpoint::builder(presets::N0).relay_mode(RelayMode::Custom(relays.map()))
        }
        Network::Local(lookup) => Endpoint::builder(presets::Minimal).address_lookup(lookup.clone()),
    };
    Ok(builder
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
/// a captive-portal wifi has a network and is not reachable.
async fn watch_reachable(endpoint: Endpoint, events: mpsc::Sender<Event>) {
    let mut status = endpoint.home_relay_status();
    let mut online = false;
    loop {
        let reachable = status.get().into_iter().any(|relay| relay.is_connected());
        if reachable != online {
            online = reachable;
            tracing::info!(online, addr = ?endpoint.addr(), "reachability");
            emit(&events, if online { Event::Online } else { Event::Offline }).await;
        }
        if status.updated().await.is_err() {
            break;
        }
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
/// `elapsed` is reported because it is not `BEAT`. A tokio timer waits on a clock that does not
/// wake a suspended CPU, so a beat that took much longer than it asked for is the measure of how
/// long the device was actually asleep — the one thing Doze otherwise hides.
async fn heartbeat(endpoint: Endpoint) {
    let mut last = Counters::read(&endpoint);
    let mut at = std::time::Instant::now();
    loop {
        tokio::time::sleep(BEAT).await;
        let (now, counters) = (std::time::Instant::now(), Counters::read(&endpoint));
        let beat = counters.since(last);
        let relay = endpoint.home_relay_status().get().into_iter().any(|relay| relay.is_connected());
        tracing::info!(
            elapsed_s = now.duration_since(at).as_secs(),
            relay,
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
            (Command::Call(peer), None) => {
                let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
                let task = outgoing(self.endpoint.clone(), peer, control_rx, self.events.clone());
                self.spawn_call(control, Some(peer), task);
            }
            // Refused, and said only in the log. It used to go out as `Ended`, which every listener
            // read as the end of the call that *is* up: the app logged that call as failed and
            // tore its media down while it carried on.
            (Command::Call(peer), Some(_)) => {
                tracing::warn!(peer = %peer.fmt_short(), "refused a call while one is up");
            }
            (Command::Answer(accept), Some(call)) => forward(call, Control::Answer(accept)).await,
            (Command::Hangup, Some(call)) => forward(call, Control::Hangup).await,
            (Command::Answer(_) | Command::Hangup, None) => tracing::debug!(?command, "no call"),
        }
    }

    fn incoming(&mut self, incoming: Incoming) {
        if self.call.is_some() {
            tokio::spawn(reply_busy(incoming));
            return;
        }
        let (control, control_rx) = mpsc::channel(CONTROL_QUEUE);
        let task = answer(incoming, control_rx, self.events.clone());
        self.spawn_call(control, None, task);
    }

    /// Runs a call task; it reports its own end, then frees the call slot.
    fn spawn_call(
        &mut self,
        control: mpsc::Sender<Control>,
        peer: Option<EndpointId>,
        task: impl Future<Output = (Option<EndpointId>, Result<EndReason, Error>)> + Send + 'static,
    ) {
        let id = self.next_call;
        self.next_call += 1;
        self.call = Some(ActiveCall { id, control });
        let (events, finished) = (self.events.clone(), self.finished.clone());
        tokio::spawn(async move {
            let (known_peer, outcome) = task.await;
            let reason = outcome.unwrap_or_else(|e| EndReason::Failed(e.to_string()));
            tracing::info!(peer = ?known_peer.or(peer), ?reason, "call ended");
            emit(&events, Event::Ended { peer: known_peer.or(peer), reason }).await;
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
async fn finish(connection: &Connection, send: &mut SendStream, signal: Signal, code: iroh::endpoint::VarInt) -> Result<(), Error> {
    protocol::send(send, signal).await?;
    if tokio::time::timeout(CLOSE_GRACE, connection.closed()).await.is_err() {
        connection.close(code, b"");
    }
    Ok(())
}

async fn outgoing(
    endpoint: Endpoint,
    peer: EndpointId,
    control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
) -> (Option<EndpointId>, Result<EndReason, Error>) {
    (Some(peer), dial(endpoint, peer, control, events).await)
}

async fn dial(
    endpoint: Endpoint,
    peer: EndpointId,
    mut control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
) -> Result<EndReason, Error> {
    emit(&events, Event::Dialing { peer }).await;
    // Dialling a key nobody is listening on has no natural end: iroh keeps trying relays and
    // holepunching for as long as it is asked to, so the deadline has to come from here.
    let connection = tokio::select! {
        connection = tokio::time::timeout(DIAL_TIMEOUT, endpoint.connect(peer, ALPN)) => match connection {
            Ok(connection) => connection?,
            Err(_) => return Ok(EndReason::DialTimeout),
        },
        Some(Control::Hangup) = control.recv() => return Ok(EndReason::LocalHangup),
    };
    let key_exchange = secure(&connection)?;
    let (mut send, recv) = connection.open_bi().await?;
    protocol::send(&mut send, Signal::Offer).await?;
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
                Some(Ok(Signal::Accept)) => {
                    active(&connection, peer, key_exchange, send, signals, &mut control, &events).await
                }
                Some(Ok(Signal::Reject)) => Ok(EndReason::Rejected),
                Some(Ok(Signal::Busy)) => Ok(EndReason::Busy),
                Some(Ok(Signal::Hangup)) | None => Ok(EndReason::RemoteHangup),
                Some(Ok(Signal::Offer | Signal::KeyframeRequest)) => {
                    protocol_error(&connection, "unexpected signal while ringing")
                }
                Some(Err(e)) => Err(e),
            },
            command = control.recv() => match command {
                Some(Control::Hangup) | None => {
                    finish(&connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
                    return Ok(EndReason::LocalHangup);
                }
                Some(Control::Answer(_)) => tracing::debug!("answer ignored on outgoing call"),
            },
        }
    }
}

async fn answer(
    incoming: Incoming,
    mut control: mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
) -> (Option<EndpointId>, Result<EndReason, Error>) {
    let connection = match accept_connection(incoming).await {
        Ok(connection) => connection,
        Err(e) => return (None, Err(e)),
    };
    let peer = connection.remote_id();
    (Some(peer), ring(connection, peer, &mut control, events).await)
}

async fn accept_connection(incoming: Incoming) -> Result<Connection, Error> {
    Ok(incoming.accept()?.await?)
}

async fn ring(
    connection: Connection,
    peer: EndpointId,
    control: &mut mpsc::Receiver<Control>,
    events: mpsc::Sender<Event>,
) -> Result<EndReason, Error> {
    let key_exchange = secure(&connection)?;
    let (mut send, recv) = connection.accept_bi().await?;
    let mut signals = spawn_reader(recv);
    match signals.recv().await {
        Some(Ok(Signal::Offer)) => {}
        Some(Err(e)) => return Err(e),
        Some(Ok(_)) | None => return protocol_error(&connection, "expected offer"),
    }
    tracing::info!(%peer, "incoming call");
    emit(&events, Event::Incoming { peer }).await;
    tokio::select! {
        signal = signals.recv() => match signal {
            Some(Ok(Signal::Hangup)) | None => Ok(EndReason::RemoteHangup),
            Some(Ok(_)) => protocol_error(&connection, "unexpected signal while ringing"),
            Some(Err(e)) => Err(e),
        },
        command = control.recv() => match command {
            Some(Control::Answer(true)) => {
                protocol::send(&mut send, Signal::Accept).await?;
                active(&connection, peer, key_exchange, send, signals, control, &events).await
            }
            Some(Control::Answer(false) | Control::Hangup) | None => {
                finish(&connection, &mut send, Signal::Reject, CLOSE_REJECTED).await?;
                Ok(EndReason::Declined)
            }
        },
    }
}

async fn active(
    connection: &Connection,
    peer: EndpointId,
    key_exchange: NamedGroup,
    mut send: SendStream,
    mut signals: mpsc::Receiver<Result<Signal, Error>>,
    control: &mut mpsc::Receiver<Control>,
    events: &mpsc::Sender<Event>,
) -> Result<EndReason, Error> {
    tracing::info!(%peer, ?key_exchange, "call connected");
    let (media, mut links) = media::start(connection)?;
    emit(events, Event::Connected { peer, key_exchange, media: Box::new(media) }).await;
    loop {
        tokio::select! {
            signal = signals.recv() => match signal {
                Some(Ok(Signal::KeyframeRequest)) => {
                    links.stats.keyframe_requests_received.fetch_add(1, Ordering::Relaxed);
                    if links.keyframe_requested.try_send(()).is_err() {
                        tracing::debug!("keyframe request already pending");
                    }
                }
                Some(Ok(Signal::Hangup)) | None => {
                    connection.close(CLOSE_HANGUP, b"");
                    return Ok(EndReason::RemoteHangup);
                }
                Some(Ok(_)) => return protocol_error(connection, "unexpected signal in call"),
                Some(Err(e)) => return Err(e),
            },
            Some(()) = links.request_keyframe.recv() => protocol::send(&mut send, Signal::KeyframeRequest).await?,
            command = control.recv() => match command {
                Some(Control::Hangup) | None => {
                    finish(connection, &mut send, Signal::Hangup, CLOSE_HANGUP).await?;
                    return Ok(EndReason::LocalHangup);
                }
                Some(Control::Answer(_)) => tracing::debug!("answer ignored during call"),
            },
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

async fn reply_busy(incoming: Incoming) {
    let outcome = async {
        let connection = accept_connection(incoming).await?;
        tracing::info!(peer = %connection.remote_id(), "busy: declining call");
        let (mut send, _recv) = connection.accept_bi().await?;
        finish(&connection, &mut send, Signal::Busy, CLOSE_BUSY).await
    };
    if let Err(e) = outcome.await {
        tracing::debug!("busy reply failed: {e}");
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
        let (node, mut events) = Node::start(SecretKey::generate(), Network::Local(lookup.clone())).await?;
        let Some(Event::Ready { id }) = events.recv().await else { bail!("node not ready") };

        let classical = Arc::new(CryptoProvider {
            kx_groups: vec![aws_lc_rs::kx_group::X25519],
            ..aws_lc_rs::default_provider()
        });
        let client = Endpoint::builder(presets::Minimal).crypto_provider(classical).address_lookup(lookup).bind().await?;
        let connection = tokio::time::timeout(TIMEOUT, client.connect(id, ALPN)).await.context("connect timed out")??;
        let closed = tokio::time::timeout(TIMEOUT, connection.closed()).await.context("not closed")?;
        assert!(
            matches!(&closed, ConnectionError::ApplicationClosed(close) if close.error_code == CLOSE_NOT_POST_QUANTUM),
            "unexpected close: {closed:?}"
        );

        client.close().await;
        node.shutdown().await;
        Ok(())
    }
}
