//! Per-call stats in the log: every [`INTERVAL`] one line with the selected path, link health and
//! media counters for that interval, and a summary when the call ends. Local only; nothing is sent
//! to the peer.
//!
//! Also how the call found its way: the addresses each side could offer for a direct path, by
//! family, when the call connects and again when it ends, and every path QUIC opens, selects and
//! closes as it happens. That is what says whether a relayed call was relayed because nobody had
//! an IPv6 address, because nobody tried it, or because something dropped it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::{Connection, PathEvent, TransportAddrUsage};
use iroh::{Endpoint, TransportAddr};
use n0_future::StreamExt;
use tokio::time::Instant;

use crate::media::{MediaStats, Route};

/// Which way a path goes: through a relay, or direct over one IP family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    Relay,
    V4,
    V6,
    Other,
}

impl Family {
    const fn of(addr: &TransportAddr) -> Self {
        match addr {
            TransportAddr::Relay(_) => Self::Relay,
            TransportAddr::Ip(ip) if is_v4(ip) => Self::V4,
            TransportAddr::Ip(_) => Self::V6,
            _ => Self::Other,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Relay => RELAY,
            Self::V4 => "ipv4",
            Self::V6 => "ipv6",
            Self::Other => "other",
        }
    }
}

/// IPv4, counting an IPv4 address carried in an IPv6 socket as what it is.
const fn is_v4(addr: &SocketAddr) -> bool {
    addr.ip().to_canonical().is_ipv4()
}

/// An IPv6 address another network could reach: not loopback, link-local or unique-local, which
/// every phone has and none of which counts as having IPv6 to offer.
const fn is_global_v6(addr: &SocketAddr) -> bool {
    match addr.ip().to_canonical() {
        std::net::IpAddr::V6(ip) => !ip.is_loopback() && !ip.is_unicast_link_local() && !ip.is_unique_local(),
        std::net::IpAddr::V4(_) => false,
    }
}

/// Ours and theirs, as each side could offer them for a direct path. Theirs is what our endpoint
/// has learned of the other phone — by lookup, and from the call itself as it traverses NATs — and
/// whether each is in use.
async fn log_candidates(endpoint: &Endpoint, connection: &Connection, when: &'static str, media: &MediaStats) {
    let ours: Vec<SocketAddr> = endpoint.addr().ip_addrs().copied().collect();
    let theirs: Vec<(SocketAddr, bool)> = endpoint
        .remote_info(connection.remote_id())
        .await
        .map(|info| {
            info.addrs()
                .filter_map(|known| match known.addr() {
                    TransportAddr::Ip(ip) => Some((*ip, matches!(known.usage(), TransportAddrUsage::Active))),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let (we_v6, they_v6) = (ours.iter().any(is_global_v6), theirs.iter().any(|(ip, _)| is_global_v6(ip)));
    tracing::info!(when, we_v6, they_v6, ?ours, ?theirs, "call candidates");
    let mut quality = media.quality.lock();
    quality.we_offered_v6 |= we_v6;
    quality.they_offered_v6 |= they_v6;
}

/// One path's life: opened, selected, closed. Logged as it happens, since the interval line only
/// sees the path in use at the moment it samples.
fn path_event(event: &PathEvent, media: &MediaStats) {
    match event {
        PathEvent::Opened { remote_addr, local_addr, .. } => {
            let family = Family::of(remote_addr);
            tracing::info!(family = family.name(), remote = ?remote_addr, local = ?local_addr, "path opened");
            let mut quality = media.quality.lock();
            match family {
                Family::V4 => quality.v4_path_opened = true,
                Family::V6 => quality.v6_path_opened = true,
                Family::Relay | Family::Other => {}
            }
        }
        PathEvent::Selected { remote_addr, .. } => {
            tracing::info!(family = Family::of(remote_addr).name(), remote = ?remote_addr, "path selected");
        }
        PathEvent::Closed { remote_addr, last_stats, .. } => tracing::info!(
            family = Family::of(remote_addr).name(),
            remote = ?remote_addr,
            rtt_ms = last_stats.rtt.as_millis(),
            sent = last_stats.udp_tx.bytes,
            received = last_stats.udp_rx.bytes,
            lost = last_stats.lost_packets,
            "path closed"
        ),
        PathEvent::Lagged { missed, .. } => tracing::debug!(missed, "path events dropped"),
        event => tracing::debug!(?event, "path event"),
    }
}

const INTERVAL: Duration = Duration::from_secs(5);
/// The one spelling of it, shared by the log line and the route the UI shows.
const RELAY: &str = "relay";
const BITS_PER_BYTE: u64 = 8;
const BYTES_PER_MB: u64 = 1_000_000;
const PERCENT: f64 = 100.0;

/// Monotonic counters sampled each interval; deltas give per-interval rates.
#[derive(Clone, Copy, Default)]
struct Counters {
    bytes_up: u64,
    bytes_down: u64,
    datagrams_up: u64,
    lost_packets: u64,
    congestion_events: u64,
    frames_sent: u64,
    frames_late: u64,
    frames_congested: u64,
    frames_received: u64,
    frames_dropped: u64,
    keyframe_asks_sent: u64,
    keyframe_asks_received: u64,
    audio_sent: u64,
    audio_received: u64,
    audio_late: u64,
    audio_fec: u64,
    audio_concealed: u64,
}

impl Counters {
    fn sample(connection: &Connection, media: &MediaStats, congestion_events: u64) -> Self {
        let link = connection.stats();
        let count = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Self {
            bytes_up: link.udp_tx.bytes,
            bytes_down: link.udp_rx.bytes,
            datagrams_up: link.udp_tx.datagrams,
            lost_packets: link.lost_packets,
            congestion_events,
            frames_sent: count(&media.frames_sent),
            frames_late: count(&media.frames_late),
            frames_congested: count(&media.frames_dropped_congested),
            frames_received: count(&media.frames_received),
            frames_dropped: count(&media.frames_dropped_received),
            keyframe_asks_sent: count(&media.keyframe_requests_sent),
            keyframe_asks_received: count(&media.keyframe_requests_received),
            audio_sent: count(&media.audio_sent),
            audio_received: count(&media.audio_received),
            audio_late: count(&media.audio_late),
            audio_fec: count(&media.audio_fec_recovered),
            audio_concealed: count(&media.audio_concealed),
        }
    }

    const fn since(&self, earlier: &Self) -> Self {
        Self {
            bytes_up: self.bytes_up.saturating_sub(earlier.bytes_up),
            bytes_down: self.bytes_down.saturating_sub(earlier.bytes_down),
            datagrams_up: self.datagrams_up.saturating_sub(earlier.datagrams_up),
            lost_packets: self.lost_packets.saturating_sub(earlier.lost_packets),
            congestion_events: self.congestion_events.saturating_sub(earlier.congestion_events),
            frames_sent: self.frames_sent.saturating_sub(earlier.frames_sent),
            frames_late: self.frames_late.saturating_sub(earlier.frames_late),
            frames_congested: self.frames_congested.saturating_sub(earlier.frames_congested),
            frames_received: self.frames_received.saturating_sub(earlier.frames_received),
            frames_dropped: self.frames_dropped.saturating_sub(earlier.frames_dropped),
            keyframe_asks_sent: self.keyframe_asks_sent.saturating_sub(earlier.keyframe_asks_sent),
            keyframe_asks_received: self.keyframe_asks_received.saturating_sub(earlier.keyframe_asks_received),
            audio_sent: self.audio_sent.saturating_sub(earlier.audio_sent),
            audio_received: self.audio_received.saturating_sub(earlier.audio_received),
            audio_late: self.audio_late.saturating_sub(earlier.audio_late),
            audio_fec: self.audio_fec.saturating_sub(earlier.audio_fec),
            audio_concealed: self.audio_concealed.saturating_sub(earlier.audio_concealed),
        }
    }
}

/// The path QUIC currently sends on.
#[derive(Clone, PartialEq, Eq)]
struct SelectedPath {
    family: Family,
    remote: String,
    rtt: Duration,
    cwnd: u64,
    mtu: u16,
    congestion_events: u64,
}

fn selected_path(connection: &Connection) -> Option<SelectedPath> {
    let paths = connection.paths();
    let path = paths.iter().find(|path| path.is_selected())?;
    let stats = path.stats();
    Some(SelectedPath {
        family: Family::of(path.remote_addr()),
        remote: format!("{:?}", path.remote_addr()),
        rtt: stats.rtt,
        cwnd: stats.cwnd,
        mtu: stats.current_mtu,
        congestion_events: stats.congestion_events,
    })
}

/// Bits per millisecond is kbit/s.
fn kbps(bytes: u64, over: Duration) -> u64 {
    let millis = u64::try_from(over.as_millis()).unwrap_or(u64::MAX).max(1);
    bytes.saturating_mul(BITS_PER_BYTE) / millis
}

/// Lossless for the per-interval counts logged here; saturates beyond `u32`.
fn float(count: u64) -> f64 {
    u32::try_from(count).map_or(f64::from(u32::MAX), f64::from)
}

fn rate(count: u64, over: Duration) -> f64 {
    float(count) / over.as_secs_f64()
}

/// Lost as a share of sent. Clamped, because the two are not counted on the same basis: losses
/// are QUIC packets aggregated over every path, while the denominator is UDP datagrams observed,
/// and a call that migrates between the relay and a direct path has produced over 100%. The raw
/// loss count is logged beside this so a nonsense ratio is visible rather than believed.
fn loss_percent(lost: u64, sent: u64) -> f64 {
    if sent == 0 { 0.0 } else { (float(lost) / float(sent) * PERCENT).min(PERCENT) }
}

/// Logs stats until the connection closes, then a summary. Spawned with the call's media.
pub(crate) async fn run(connection: Connection, endpoint: Endpoint, media: Arc<MediaStats>) {
    let started = Instant::now();
    let mut tick = tokio::time::interval_at(started + INTERVAL, INTERVAL);
    let mut path = selected_path(&connection);
    let first = Counters::sample(&connection, &media, path.as_ref().map_or(0, |p| p.congestion_events));
    let (mut last, mut last_at) = (first, started);
    let (mut rtt_total, mut rtt_max, mut samples) = (Duration::ZERO, Duration::ZERO, 0u32);
    media.quality.lock().paths_recorded = true;
    let mut events = connection.path_events();
    // The first choice is made before anything here is listening, and the events only say what
    // changes after it, so the path the call started on is said once, here.
    if let Some(p) = &path {
        tracing::info!(family = p.family.name(), remote = p.remote, rtt_ms = p.rtt.as_millis(), "path selected at start");
    }
    log_candidates(&endpoint, &connection, "connected", &media).await;
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            Some(event) = events.next() => {
                path_event(&event, &media);
                continue;
            }
            _ = tick.tick() => {}
        }
        let now = Instant::now();
        let current = selected_path(&connection);
        if current.as_ref().map(|p| (p.family, &p.remote)) != path.as_ref().map(|p| (p.family, &p.remote)) {
            let (kind, remote) = current.as_ref().map_or(("none", ""), |p| (p.family.name(), p.remote.as_str()));
            tracing::info!(kind, remote, "call path changed");
        }
        path = current.or(path);
        let Some(p) = &path else { continue };
        media.set_route(if p.family == Family::Relay { Route::Relay } else { Route::Direct });
        let counters = Counters::sample(&connection, &media, p.congestion_events);
        let delta = counters.since(&last);
        let over = now.duration_since(last_at);
        rtt_total += p.rtt;
        rtt_max = rtt_max.max(p.rtt);
        samples += 1;
        // The same figures the line below logs, kept as a spread for the call's own record.
        {
            let mut quality = media.quality.lock();
            quality.fps_out.add(rate(delta.frames_sent, over));
            quality.fps_in.add(rate(delta.frames_received, over));
            quality.kbps_up.add(float(kbps(delta.bytes_up, over)));
            quality.kbps_down.add(float(kbps(delta.bytes_down, over)));
            quality.rtt_ms.add(float(u64::try_from(p.rtt.as_millis()).unwrap_or(u64::MAX)));
            let counter = match p.family {
                Family::Relay => Some(&mut quality.relayed_samples),
                Family::V4 => Some(&mut quality.direct_v4_samples),
                Family::V6 => Some(&mut quality.direct_v6_samples),
                Family::Other => None,
            };
            if let Some(counter) = counter {
                *counter = counter.saturating_add(1);
            }
        }
        tracing::info!(
            path = p.family.name(),
            rtt_ms = p.rtt.as_millis(),
            cwnd = p.cwnd,
            mtu = p.mtu,
            loss_pct = format!("{:.1}", loss_percent(delta.lost_packets, delta.datagrams_up)),
            lost = delta.lost_packets,
            congestion_events = delta.congestion_events,
            up_kbps = kbps(delta.bytes_up, over),
            down_kbps = kbps(delta.bytes_down, over),
            fps_out = format!("{:.1}", rate(delta.frames_sent, over)),
            fps_in = format!("{:.1}", rate(delta.frames_received, over)),
            late = delta.frames_late,
            congested = delta.frames_congested,
            dropped = delta.frames_dropped,
            keyframe_asks_sent = delta.keyframe_asks_sent,
            keyframe_asks_received = delta.keyframe_asks_received,
            audio_out = delta.audio_sent,
            audio_in = delta.audio_received,
            audio_late = delta.audio_late,
            audio_fec = delta.audio_fec,
            audio_concealed = delta.audio_concealed,
            "call stats"
        );
        (last, last_at) = (counters, now);
    }
    // Again at the end: what the call learned of each side along the way shows up here.
    log_candidates(&endpoint, &connection, "ended", &media).await;
    let total = Counters::sample(&connection, &media, last.congestion_events).since(&first);
    tracing::info!(
        duration_s = started.elapsed().as_secs(),
        last_path = path.as_ref().map_or("none", |p| p.family.name()),
        rtt_avg_ms = rtt_total.checked_div(samples).unwrap_or_default().as_millis(),
        rtt_max_ms = rtt_max.as_millis(),
        loss_pct = format!("{:.1}", loss_percent(total.lost_packets, total.datagrams_up)),
        lost = total.lost_packets,
        mb_up = total.bytes_up / BYTES_PER_MB,
        mb_down = total.bytes_down / BYTES_PER_MB,
        frames_sent = total.frames_sent,
        frames_received = total.frames_received,
        late = total.frames_late,
        congested = total.frames_congested,
        dropped = total.frames_dropped,
        keyframe_asks_sent = total.keyframe_asks_sent,
        keyframe_asks_received = total.keyframe_asks_received,
        audio_out = total.audio_sent,
        audio_in = total.audio_received,
        audio_late = total.audio_late,
        audio_fec = total.audio_fec,
        audio_concealed = total.audio_concealed,
        "call summary"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(text: &str) -> SocketAddr {
        text.parse().unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    #[test]
    fn a_path_is_named_by_its_real_family() {
        assert_eq!(Family::of(&TransportAddr::Ip(addr("203.0.113.7:21783"))), Family::V4);
        assert_eq!(Family::of(&TransportAddr::Ip(addr("[2001:db8::1]:39305"))), Family::V6);
        // An IPv4 address carried in an IPv6 socket is IPv4.
        assert_eq!(Family::of(&TransportAddr::Ip(addr("[::ffff:192.168.31.90]:59523"))), Family::V4);
    }

    #[test]
    fn only_a_global_ipv6_address_counts_as_one_to_offer() {
        assert!(is_global_v6(&addr("[2001:db8::1]:39305")));
        assert!(!is_global_v6(&addr("[fe80::1]:1")));
        assert!(!is_global_v6(&addr("[fd00::1]:1")));
        assert!(!is_global_v6(&addr("[::1]:1")));
        assert!(!is_global_v6(&addr("192.168.31.90:59523")));
    }

    #[test]
    fn rates_per_interval() {
        let second = Duration::from_secs(1);
        assert_eq!(kbps(250_000, second), 2000);
        assert_eq!(kbps(1, Duration::ZERO), BITS_PER_BYTE);
        assert!((rate(30, second) - 30.0).abs() < f64::EPSILON);
        assert!((loss_percent(5, 100) - 5.0).abs() < f64::EPSILON);
        assert!(loss_percent(3, 0).abs() < f64::EPSILON);
    }

    #[test]
    fn deltas_never_underflow() {
        let later = Counters { frames_sent: 40, bytes_up: 10, ..Counters::default() };
        let earlier = Counters { frames_sent: 10, bytes_up: 20, ..Counters::default() };
        let delta = later.since(&earlier);
        assert_eq!((delta.frames_sent, delta.bytes_up), (30, 0));
    }
}
