//! Call quality: the most a call sends, picture and voice together, as one of five presets,
//! chosen separately for Wi-Fi and for mobile data. A cap, not a target: a struggling network
//! still gets less.
//!
//! The steps are the 16:9 sizes cameras actually output (640×360, 960×540, 1280×720,
//! 1920×1080 — the camera feeds the encoder's surface, so a size it cannot produce cannot be
//! sent) and the rungs video calling ladders use. Not every phone manages every step; the app
//! asks the camera and the encoder which, and offers only those.

use crate::Error;
use crate::quality::VideoTarget;
use crate::settings::Settings;

/// Where the chosen preset for each network is kept.
pub const WIFI: &str = "quality-wifi";
pub const MOBILE: &str = "quality-mobile";

/// The kind of network a call is on, as far as what it costs to send goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// Wi-Fi or Ethernet: nobody is counting the megabytes.
    Wifi,
    /// Anything metered: mobile data, mostly.
    Mobile,
}

impl Network {
    const fn key(self) -> &'static str {
        match self {
            Self::Wifi => WIFI,
            Self::Mobile => MOBILE,
        }
    }

    /// Until someone chooses: high on Wi-Fi, low on mobile data.
    pub const fn default_preset(self) -> Preset {
        match self {
            Self::Wifi => Preset::High,
            Self::Mobile => Preset::Low,
        }
    }
}

/// Lowest to highest, so a step down is the one before.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Preset {
    Lowest,
    Low,
    /// What every call sent before there was a choice.
    Balanced,
    High,
    /// The same picture as High, twice as smooth.
    Highest,
}

impl Preset {
    pub const ALL: [Self; 5] = [Self::Lowest, Self::Low, Self::Balanced, Self::High, Self::Highest];

    /// The picture: the most it sends.
    pub const fn video(self) -> VideoTarget {
        match self {
            Self::Lowest => VideoTarget { width: 640, height: 360, fps: 15, kbps: 350 },
            Self::Low => VideoTarget { width: 960, height: 540, fps: 24, kbps: 900 },
            Self::Balanced => VideoTarget { width: 1280, height: 720, fps: 30, kbps: 2000 },
            Self::High => VideoTarget { width: 1920, height: 1080, fps: 30, kbps: 4000 },
            Self::Highest => VideoTarget { width: 1920, height: 1080, fps: 60, kbps: 6000 },
        }
    }

    /// Opus's bitrate for the voice, in bits a second. Past 48 kbps a voice gains nothing.
    pub const fn voice_bps(self) -> i32 {
        match self {
            Self::Lowest => 16_000,
            Self::Low => 24_000,
            Self::Balanced => 32_000,
            Self::High | Self::Highest => 48_000,
        }
    }

    /// Roughly what a minute of it costs, picture and voice, before the network's own overhead:
    /// the figure the picker shows.
    pub const fn megabytes_a_minute(self) -> u32 {
        const BITS_PER_BYTE: u32 = 8;
        const SECONDS: u32 = 60;
        const KILO: u32 = 1000;
        const BPS_PER_KBPS: i32 = 1000;
        let voice_kbps = self.voice_bps() / BPS_PER_KBPS;
        // Whole kbps, positive by construction.
        let kbps = self.video().kbps + voice_kbps.unsigned_abs();
        (kbps * SECONDS / BITS_PER_BYTE).div_ceil(KILO)
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Lowest => "lowest",
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::High => "high",
            Self::Highest => "highest",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.key() == key)
    }

    /// What is chosen for `network`, or its default if nothing is (or something unreadable).
    pub fn chosen(settings: &Settings, network: Network) -> Self {
        settings.get(network.key()).and_then(|key| Self::from_key(&key)).unwrap_or(network.default_preset())
    }

    pub fn choose(self, settings: &Settings, network: Network) -> Result<(), Error> {
        settings.set(network.key(), self.key())
    }

    /// This, or the highest step below it this phone can send; the lowest if it can send none,
    /// which is still a call.
    pub fn within(self, supported: &[Self]) -> Self {
        supported.iter().copied().filter(|step| *step <= self).max().unwrap_or(Self::Lowest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_reads_back_as_itself() {
        for preset in Preset::ALL {
            assert_eq!(Preset::from_key(preset.key()), Some(preset));
        }
        assert_eq!(Preset::from_key("ultra"), None);
    }

    #[test]
    fn each_step_sends_more() {
        for pair in Preset::ALL.windows(2) {
            let (less, more) = (pair[0], pair[1]);
            assert!(less < more);
            assert!(less.video().kbps < more.video().kbps);
            assert!(less.voice_bps() <= more.voice_bps());
            assert!(less.megabytes_a_minute() < more.megabytes_a_minute());
        }
    }

    #[test]
    fn balanced_is_what_calls_sent_before() {
        let video = Preset::Balanced.video();
        assert_eq!((video.width, video.height, video.fps, video.kbps), (1280, 720, 30, 2000));
        assert_eq!(Preset::Balanced.voice_bps(), 32_000);
        assert_eq!(Preset::Balanced.megabytes_a_minute(), 16);
    }

    #[test]
    fn wifi_starts_high_and_mobile_low() {
        assert_eq!(Network::Wifi.default_preset(), Preset::High);
        assert_eq!(Network::Mobile.default_preset(), Preset::Low);
    }

    #[test]
    fn a_step_the_phone_cannot_send_falls_to_the_next_below() {
        let up_to_720 = [Preset::Lowest, Preset::Low, Preset::Balanced];
        assert_eq!(Preset::Highest.within(&up_to_720), Preset::Balanced);
        assert_eq!(Preset::Low.within(&up_to_720), Preset::Low);
        assert_eq!(Preset::High.within(&[]), Preset::Lowest);
    }
}
