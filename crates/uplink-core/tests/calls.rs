//! End-to-end call flows between real nodes on loopback (no relays, no internet).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use tokio::sync::mpsc::Receiver;
use uplink_core::crypto::is_post_quantum;
use uplink_core::media::MediaSession;
use uplink_core::node::{Command, EndReason, Event, Network, Node};
use uplink_core::{EndpointId, MemoryLookup};

const EVENT_TIMEOUT: Duration = Duration::from_secs(15);

struct Peer {
    id: EndpointId,
    node: Node,
    events: Receiver<Event>,
}

impl Peer {
    async fn start(lookup: &MemoryLookup) -> Result<Self> {
        let (node, mut events) = Node::start(SecretKey::generate(), Network::Local(lookup.clone())).await?;
        let id = match next(&mut events).await? {
            Event::Ready { id } => id,
            other => bail!("expected Ready, got {other:?}"),
        };
        Ok(Self { id, node, events })
    }

    /// Skips events until one matches `wanted`.
    async fn expect(&mut self, what: &str, wanted: impl Fn(&Event) -> bool) -> Result<Event> {
        loop {
            let event = next(&mut self.events).await.with_context(|| format!("waiting for {what}"))?;
            if wanted(&event) {
                return Ok(event);
            }
        }
    }

    async fn ended(&mut self) -> Result<EndReason> {
        match self.expect("call end", |e| matches!(e, Event::Ended { .. })).await? {
            Event::Ended { reason, .. } => Ok(reason),
            other => bail!("expected Ended, got {other:?}"),
        }
    }
}

async fn next(events: &mut Receiver<Event>) -> Result<Event> {
    tokio::time::timeout(EVENT_TIMEOUT, events.recv()).await.context("timed out")?.context("event channel closed")
}

/// `caller` calls `callee`, who sees it ring.
async fn ring(caller: &mut Peer, callee: &mut Peer) -> Result<()> {
    caller.node.send(Command::Call(callee.id)).await?;
    let from = caller.id;
    callee.expect("incoming call", |e| matches!(e, Event::Incoming { peer } if *peer == from)).await?;
    caller.expect("ringing", |e| matches!(e, Event::Ringing { .. })).await?;
    Ok(())
}

/// Waits for `peer` to connect to `other` post-quantum and returns its media session.
async fn connected_media(peer: &mut Peer, other: EndpointId) -> Result<MediaSession> {
    let connected = peer.expect("connected", |e| matches!(e, Event::Connected { .. })).await?;
    let Event::Connected { peer: remote, key_exchange, media } = connected else { bail!("expected Connected") };
    assert_eq!(remote, other);
    assert!(is_post_quantum(key_exchange), "negotiated {key_exchange:?}");
    Ok(media)
}

/// Rings and answers; returns (caller, callee) media sessions.
async fn connect_with_media(caller: &mut Peer, callee: &mut Peer) -> Result<(MediaSession, MediaSession)> {
    ring(caller, callee).await?;
    callee.node.send(Command::Answer(true)).await?;
    let (caller_id, callee_id) = (caller.id, callee.id);
    Ok((connected_media(caller, callee_id).await?, connected_media(callee, caller_id).await?))
}

async fn connect(caller: &mut Peer, callee: &mut Peer) -> Result<()> {
    connect_with_media(caller, callee).await.map(|_| ())
}

#[tokio::test(flavor = "multi_thread")]
async fn answered_call_is_post_quantum_and_hangs_up_cleanly() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    connect(&mut alice, &mut bob).await?;

    alice.node.send(Command::Hangup).await?;
    assert!(matches!(alice.ended().await?, EndReason::LocalHangup));
    assert!(matches!(bob.ended().await?, EndReason::RemoteHangup));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn declined_call_is_rejected_for_the_caller() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    ring(&mut alice, &mut bob).await?;

    bob.node.send(Command::Answer(false)).await?;
    assert!(matches!(alice.ended().await?, EndReason::Rejected));
    assert!(matches!(bob.ended().await?, EndReason::Declined));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_can_cancel_while_ringing() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    ring(&mut alice, &mut bob).await?;

    alice.node.send(Command::Hangup).await?;
    assert!(matches!(alice.ended().await?, EndReason::LocalHangup));
    assert!(matches!(bob.ended().await?, EndReason::RemoteHangup));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn third_caller_gets_busy() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    let mut carol = Peer::start(&lookup).await?;
    connect(&mut alice, &mut bob).await?;

    carol.node.send(Command::Call(bob.id)).await?;
    assert!(matches!(carol.ended().await?, EndReason::Busy));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn second_outgoing_call_fails_while_in_a_call() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    let carol = Peer::start(&lookup).await?;
    connect(&mut alice, &mut bob).await?;

    alice.node.send(Command::Call(carol.id)).await?;
    assert!(matches!(alice.ended().await?, EndReason::Failed(_)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_mid_call_hangs_up_the_peer() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    connect(&mut alice, &mut bob).await?;

    alice.node.shutdown().await;
    assert!(matches!(bob.ended().await?, EndReason::RemoteHangup));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_commands_reach_the_node_until_it_stops() -> Result<()> {
    let lookup = MemoryLookup::new();
    let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
    let handle = alice.node.handle();
    handle.try_send(Command::Call(bob.id))?;
    bob.expect("incoming call", |e| matches!(e, Event::Incoming { .. })).await?;
    handle.try_send(Command::Hangup)?;
    assert!(matches!(alice.ended().await?, EndReason::LocalHangup));

    alice.node.shutdown().await;
    assert!(matches!(handle.try_send(Command::Hangup), Err(uplink_core::Error::NodeStopped)));
    Ok(())
}

mod media {
    use uplink_core::media::Frame;

    use super::*;

    const FRAMES: u64 = 30;
    const FRAME_BYTES: usize = 32 * 1024;
    /// 30 fps, like a real encoder.
    const FRAME_INTERVAL: Duration = Duration::from_millis(33);
    /// A receiver without a keyframe re-requests at this pace.
    const KEYFRAME_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn frame(id: u64, keyframe: bool) -> Frame {
        let fill = u8::try_from(id % u64::from(u8::MAX)).unwrap_or_default();
        Frame { capture_micros: id, keyframe, config: false, data: vec![fill; FRAME_BYTES] }
    }

    async fn connected() -> Result<(Peer, Peer, MediaSession, MediaSession)> {
        let lookup = MemoryLookup::new();
        let (mut alice, mut bob) = (Peer::start(&lookup).await?, Peer::start(&lookup).await?);
        let (alice_media, bob_media) = connect_with_media(&mut alice, &mut bob).await?;
        Ok((alice, bob, alice_media, bob_media))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn frames_arrive_in_order_and_intact() -> Result<()> {
        let (_alice, _bob, mut sender, mut receiver) = connected().await?;
        // Consume concurrently, like a decoder: a full queue would count as a lost frame.
        let decoder = tokio::spawn(async move {
            let mut received = Vec::new();
            for _ in 0..FRAMES {
                let frame = tokio::time::timeout(EVENT_TIMEOUT, receiver.incoming_video.recv())
                    .await
                    .context("frame timed out")?
                    .context("video channel closed")?;
                received.push(frame);
            }
            Ok::<_, anyhow::Error>(received)
        });
        for id in 0..FRAMES {
            sender.video.send(frame(id, id == 0));
            tokio::time::sleep(FRAME_INTERVAL).await;
        }
        let expected: Vec<Frame> = (0..FRAMES).map(|id| frame(id, id == 0)).collect();
        assert_eq!(decoder.await??, expected);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn receiver_without_keyframe_asks_the_sender() -> Result<()> {
        let (_alice, _bob, mut sender, _receiver) = connected().await?;
        sender.video.send(frame(0, false));
        tokio::time::timeout(KEYFRAME_REQUEST_TIMEOUT, sender.keyframe_requests.recv())
            .await
            .context("no keyframe request")?
            .context("request channel closed")?;
        Ok(())
    }
}
