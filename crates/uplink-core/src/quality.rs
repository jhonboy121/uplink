//! How a call went, kept with it in the log: the video it aimed for, how its frame rate, speed and
//! round trip spread across the call, and what each side sent, received, lost and repaired.
//!
//! Telemetry adds a sample each interval while the call runs; the counts are read once, when it
//! ends. Stored as one protobuf message in one column, so the log's schema does not grow a column
//! per figure, and a figure added later is a new tag that older rows simply read as zero — the
//! same rules as the wire (`docs/ref/wire.md`): tags are never reused or renumbered.

use prost::Message;

/// Least, most and mean of a value sampled through a call.
#[derive(Clone, Copy, PartialEq, Message)]
pub struct Spread {
    #[prost(double, tag = "1")]
    pub min: f64,
    #[prost(double, tag = "2")]
    pub max: f64,
    #[prost(double, tag = "3")]
    total: f64,
    #[prost(uint32, tag = "4")]
    samples: u32,
}

impl Spread {
    pub fn add(&mut self, value: f64) {
        if self.samples == 0 {
            (self.min, self.max) = (value, value);
        } else {
            (self.min, self.max) = (self.min.min(value), self.max.max(value));
        }
        self.total += value;
        self.samples = self.samples.saturating_add(1);
    }

    /// `None` for a call too short to have been sampled.
    pub fn mean(&self) -> Option<f64> {
        (self.samples > 0).then(|| self.total / f64::from(self.samples))
    }
}

/// The video a call was set up to send, whatever the network then allowed.
#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct VideoTarget {
    #[prost(uint32, tag = "1")]
    pub width: u32,
    #[prost(uint32, tag = "2")]
    pub height: u32,
    #[prost(uint32, tag = "3")]
    pub fps: u32,
    #[prost(uint32, tag = "4")]
    pub kbps: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct VideoCounts {
    #[prost(uint64, tag = "1")]
    pub sent: u64,
    #[prost(uint64, tag = "2")]
    pub received: u64,
    /// Ours, reset after missing their deadline.
    #[prost(uint64, tag = "3")]
    pub late: u64,
    /// Ours, never sent: too many already in flight.
    #[prost(uint64, tag = "4")]
    pub congested: u64,
    /// Theirs, arrived but useless: stale, or after a gap.
    #[prost(uint64, tag = "5")]
    pub discarded: u64,
    #[prost(uint64, tag = "6")]
    pub keyframe_asks_sent: u64,
    #[prost(uint64, tag = "7")]
    pub keyframe_asks_received: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Message)]
pub struct AudioCounts {
    #[prost(uint64, tag = "1")]
    pub sent: u64,
    #[prost(uint64, tag = "2")]
    pub received: u64,
    /// Ours, never sent: the datagram buffer was full.
    #[prost(uint64, tag = "3")]
    pub not_sent: u64,
    /// Theirs, after their playout time.
    #[prost(uint64, tag = "4")]
    pub late: u64,
    /// Theirs, lost and rebuilt from the next packet's FEC.
    #[prost(uint64, tag = "5")]
    pub rebuilt: u64,
    /// Theirs, lost and filled in by Opus.
    #[prost(uint64, tag = "6")]
    pub concealed: u64,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct Quality {
    #[prost(message, optional, tag = "1")]
    pub target: Option<VideoTarget>,
    #[prost(message, required, tag = "2")]
    pub fps_out: Spread,
    #[prost(message, required, tag = "3")]
    pub fps_in: Spread,
    /// On the wire, overhead and all: what the network carried, not what the codecs made.
    #[prost(message, required, tag = "4")]
    pub kbps_up: Spread,
    #[prost(message, required, tag = "5")]
    pub kbps_down: Spread,
    #[prost(message, required, tag = "6")]
    pub rtt_ms: Spread,
    /// Samples taken while the path was a relay; a share of `rtt_ms`'s samples.
    #[prost(uint32, tag = "7")]
    pub relayed_samples: u32,
    #[prost(message, required, tag = "8")]
    pub video: VideoCounts,
    #[prost(message, required, tag = "9")]
    pub audio: AudioCounts,
    /// Set by builds that record the fields below, so a call from before them reads as unknown
    /// rather than as "no IPv6" — a missing field reads as its default, and false is not unknown.
    #[prost(bool, tag = "10")]
    pub paths_recorded: bool,
    /// Samples taken while the path was direct, by address family; with `relayed_samples`, a
    /// share of `rtt_ms`'s samples.
    #[prost(uint32, tag = "11")]
    pub direct_v4_samples: u32,
    #[prost(uint32, tag = "12")]
    pub direct_v6_samples: u32,
    /// Whether each side had a global IPv6 address to offer the other.
    #[prost(bool, tag = "13")]
    pub we_offered_v6: bool,
    #[prost(bool, tag = "14")]
    pub they_offered_v6: bool,
    /// Whether a direct path of each family ever opened, used or not.
    #[prost(bool, tag = "15")]
    pub v4_path_opened: bool,
    #[prost(bool, tag = "16")]
    pub v6_path_opened: bool,
    /// Times the connection was lost mid-call, and times the call carried on over a new one: what
    /// tells a bad call from a bad network afterwards.
    #[prost(uint32, tag = "17")]
    pub drops: u32,
    #[prost(uint32, tag = "18")]
    pub rejoins: u32,
    /// The bitrate rate control set our encoder to, sampled with the rest: under `target`'s while
    /// the path could not carry it.
    #[prost(message, required, tag = "19")]
    pub video_kbps: Spread,
    /// Times rate control changed the picture's size or frame rate.
    #[prost(uint32, tag = "20")]
    pub step_changes: u32,
}

impl Quality {
    /// How much of the call went through a relay, from 0 to 1; `None` if it was never sampled.
    pub fn relayed_share(&self) -> Option<f64> {
        self.share(self.relayed_samples)
    }

    /// How much of the call went direct over IPv4, and over IPv6, from 0 to 1.
    pub fn direct_shares(&self) -> Option<(f64, f64)> {
        Some((self.share(self.direct_v4_samples)?, self.share(self.direct_v6_samples)?))
    }

    fn share(&self, samples: u32) -> Option<f64> {
        (self.rtt_ms.samples > 0).then(|| f64::from(samples) / f64::from(self.rtt_ms.samples))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        self.encode_to_vec()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, prost::DecodeError> {
        Self::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spread_keeps_its_ends_and_its_mean() {
        let mut spread = Spread::default();
        assert_eq!(spread.mean(), None);
        for value in [30.0, 12.0, 27.0] {
            spread.add(value);
        }
        assert_eq!((spread.min, spread.max), (12.0, 30.0));
        assert_eq!(spread.mean(), Some(23.0));
    }

    #[test]
    fn a_summary_survives_the_round_trip() -> Result<(), prost::DecodeError> {
        let mut quality = Quality {
            target: Some(VideoTarget { width: 1280, height: 720, fps: 30, kbps: 2000 }),
            relayed_samples: 1,
            ..Quality::default()
        };
        quality.fps_out.add(29.5);
        quality.rtt_ms.add(113.0);
        quality.rtt_ms.add(618.0);
        quality.audio.rebuilt = 12;
        assert_eq!(Quality::from_bytes(&quality.to_bytes())?, quality);
        assert_eq!(quality.relayed_share(), Some(0.5));
        Ok(())
    }
}
