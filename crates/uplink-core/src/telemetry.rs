//! Per-call stats in the log: every [`INTERVAL`] one line with the selected path, link health and
//! media counters for that interval, and a summary when the call ends. Local only; nothing is sent
//! to the peer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iroh::endpoint::Connection;
use tokio::time::Instant;

use crate::media::{MediaStats, Route};

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
#[derive(Clone, Default, PartialEq, Eq)]
struct SelectedPath {
    /// "direct" or "relay".
    kind: &'static str,
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
        kind: if path.is_relay() { RELAY } else { "direct" },
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
pub(crate) async fn run(connection: Connection, media: Arc<MediaStats>) {
    let started = Instant::now();
    let mut tick = tokio::time::interval_at(started + INTERVAL, INTERVAL);
    let mut path = selected_path(&connection);
    let first = Counters::sample(&connection, &media, path.as_ref().map_or(0, |p| p.congestion_events));
    let (mut last, mut last_at) = (first, started);
    let (mut rtt_total, mut rtt_max, mut samples) = (Duration::ZERO, Duration::ZERO, 0u32);
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            _ = tick.tick() => {}
        }
        let now = Instant::now();
        let current = selected_path(&connection);
        if current.as_ref().map(|p| (p.kind, &p.remote)) != path.as_ref().map(|p| (p.kind, &p.remote)) {
            let (kind, remote) = current.as_ref().map_or(("none", ""), |p| (p.kind, p.remote.as_str()));
            tracing::info!(kind, remote, "call path changed");
        }
        path = current.or(path);
        let Some(p) = &path else { continue };
        media.set_route(if p.kind == RELAY { Route::Relay } else { Route::Direct });
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
            if p.kind == RELAY {
                quality.relayed_samples = quality.relayed_samples.saturating_add(1);
            }
        }
        tracing::info!(
            path = p.kind,
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
    let total = Counters::sample(&connection, &media, last.congestion_events).since(&first);
    tracing::info!(
        duration_s = started.elapsed().as_secs(),
        last_path = path.as_ref().map_or("none", |p| p.kind),
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
