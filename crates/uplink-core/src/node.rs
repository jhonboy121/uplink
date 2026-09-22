//! Call engine: owns the iroh endpoint and handles one call at a time.
//!
//! [`Node::start`] binds the endpoint and returns the node (commands in) plus an event receiver.
//! Each call runs in its own task; signals are read by a separate reader task so `select!`
//! never cancels a half-read frame.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use iroh::endpoint::{Connection, Incoming, SendStream, presets};
use iroh::address_lookup::MemoryLookup;
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use rustls::NamedGroup;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::protocol::{
    self, ALPN, CLOSE_BUSY, CLOSE_HANGUP, CLOSE_NOT_POST_QUANTUM, CLOSE_PROTOCOL, CLOSE_REJECTED, Signal,
};
use crate::{EndpointId, Error, crypto};

const COMMAND_QUEUE: usize = 16;
const EVENT_QUEUE: usize = 64;
const CONTROL_QUEUE: usize = 4;
const SIGNAL_QUEUE: usize = 4;
/// Time the peer gets to close the connection after our final signal.
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Time an active call gets to hang up cleanly when the node shuts down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Call(EndpointId),
    Answer(bool),
    Hangup,
}

#[derive(Clone, Debug)]
pub enum Event {
    Ready { id: EndpointId },
    /// Connected to the home relay; reachable by other peers.
    Online,
    Dialing { peer: EndpointId },
    /// Our offer reached the peer; waiting for them to answer.
    Ringing { peer: EndpointId },
    Incoming { peer: EndpointId },
    /// Always post-quantum: other key exchanges are refused.
    Connected { peer: EndpointId, key_exchange: NamedGroup },
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
            Network::N0 => drop(tokio::spawn(announce_online(endpoint.clone(), events.clone()))),
            Network::Local(lookup) => lookup.add_endpoint_info(loopback_addr(&endpoint)),
        }
        let (finished, finished_rx) = mpsc::channel(CONTROL_QUEUE);
        let engine = Engine { endpoint, events, call: None, next_call: 0, finished, finished_rx };
        Ok((Self { commands, engine: tokio::spawn(engine.run(commands_rx)) }, events_rx))
    }

    pub async fn send(&self, command: Command) -> Result<(), Error> {
        self.commands.send(command).await.map_err(|_| Error::NodeStopped)
    }

    /// Ends any call and closes the endpoint.
    pub async fn shutdown(self) {
        drop(self.commands);
        if let Err(e) = self.engine.await {
            tracing::error!("node engine: {e}");
        }
    }
}

/// Where peers are found.
#[derive(Clone, Debug)]
pub enum Network {
    /// n0 relays plus DNS/pkarr address lookup.
    N0,
    /// Loopback only; nodes find each other through a shared in-memory lookup (tests, local demos).
    Local(MemoryLookup),
}

async fn bind(secret: SecretKey, network: &Network) -> Result<Endpoint, Error> {
    let builder = match network {
        Network::N0 => Endpoint::builder(presets::N0),
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

async fn announce_online(endpoint: Endpoint, events: mpsc::Sender<Event>) {
    endpoint.online().await;
    tracing::info!(addr = ?endpoint.addr(), "online");
    emit(&events, Event::Online).await;
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
            (Command::Call(peer), Some(_)) => {
                let reason = EndReason::Failed("already in a call".into());
                emit(&self.events, Event::Ended { peer: Some(peer), reason }).await;
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
    let connection = tokio::select! {
        connection = endpoint.connect(peer, ALPN) => connection?,
        Some(Control::Hangup) = control.recv() => return Ok(EndReason::LocalHangup),
    };
    let key_exchange = secure(&connection)?;
    let (mut send, recv) = connection.open_bi().await?;
    protocol::send(&mut send, Signal::Offer).await?;
    let mut signals = spawn_reader(recv);
    emit(&events, Event::Ringing { peer }).await;
    loop {
        tokio::select! {
            signal = signals.recv() => return match signal {
                Some(Ok(Signal::Accept)) => {
                    active(&connection, peer, key_exchange, send, signals, &mut control, &events).await
                }
                Some(Ok(Signal::Reject)) => Ok(EndReason::Rejected),
                Some(Ok(Signal::Busy)) => Ok(EndReason::Busy),
                Some(Ok(Signal::Hangup)) | None => Ok(EndReason::RemoteHangup),
                Some(Ok(Signal::Offer)) => protocol_error(&connection, "offer from callee"),
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
    emit(events, Event::Connected { peer, key_exchange }).await;
    loop {
        tokio::select! {
            signal = signals.recv() => return match signal {
                Some(Ok(Signal::Hangup)) | None => {
                    connection.close(CLOSE_HANGUP, b"");
                    Ok(EndReason::RemoteHangup)
                }
                Some(Ok(_)) => protocol_error(connection, "unexpected signal in call"),
                Some(Err(e)) => Err(e),
            },
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
