//! How a call went, kept with it in the log: the video it aimed for, how its frame rate, speed and
//! round trip spread across the call, and what each side sent, received, lost and repaired.
//!
//! Telemetry adds a sample each interval while the call runs; the counts are read once, when it
//! ends. Stored as postcard in one column, so the log's schema does not grow a column per figure.
//! Postcard is not self-describing: a row written by a build with a different shape reads back as
//! no summary at all, which is the honest answer for it.

use serde::{Deserialize, Serialize};

/// Least, most and mean of a value sampled through a call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spread {
    pub min: f64,
    pub max: f64,
    total: f64,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoTarget {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCounts {
    pub sent: u64,
    pub received: u64,
    /// Ours, reset after missing their deadline.
    pub late: u64,
    /// Ours, never sent: too many already in flight.
    pub congested: u64,
    /// Theirs, arrived but useless: stale, or after a gap.
    pub discarded: u64,
    pub keyframe_asks_sent: u64,
    pub keyframe_asks_received: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCounts {
    pub sent: u64,
    pub received: u64,
    /// Ours, never sent: the datagram buffer was full.
    pub not_sent: u64,
    /// Theirs, after their playout time.
    pub late: u64,
    /// Theirs, lost and rebuilt from the next packet's FEC.
    pub rebuilt: u64,
    /// Theirs, lost and filled in by Opus.
    pub concealed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quality {
    pub target: Option<VideoTarget>,
    pub fps_out: Spread,
    pub fps_in: Spread,
    /// On the wire, overhead and all: what the network carried, not what the codecs made.
    pub kbps_up: Spread,
    pub kbps_down: Spread,
    pub rtt_ms: Spread,
    /// Samples taken while the path was a relay; a share of `rtt_ms`'s samples.
    pub relayed_samples: u32,
    pub video: VideoCounts,
    pub audio: AudioCounts,
}

impl Quality {
    /// How much of the call went through a relay, from 0 to 1; `None` if it was never sampled.
    pub fn relayed_share(&self) -> Option<f64> {
        (self.rtt_ms.samples > 0).then(|| f64::from(self.relayed_samples) / f64::from(self.rtt_ms.samples))
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, postcard::Error> {
        postcard::from_bytes(bytes)
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
    fn a_summary_survives_the_round_trip() -> Result<(), postcard::Error> {
        let mut quality = Quality {
            target: Some(VideoTarget { width: 1280, height: 720, fps: 30, kbps: 2000 }),
            relayed_samples: 1,
            ..Quality::default()
        };
        quality.fps_out.add(29.5);
        quality.rtt_ms.add(113.0);
        quality.rtt_ms.add(618.0);
        quality.audio.rebuilt = 12;
        assert_eq!(Quality::from_bytes(&quality.to_bytes()?)?, quality);
        assert_eq!(quality.relayed_share(), Some(0.5));
        Ok(())
    }
}
