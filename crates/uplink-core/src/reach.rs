//! What the reachability chip says: one answer built from whether a home relay is connected and
//! whether the platform has a network at all, with enough patience that neither flickers.
//!
//! Pure: every input carries the time it happened, and [`Reachability::next_change`] says when the
//! answer could change by itself, so the node drives it with one timer and the tests with none.

use std::time::Duration;

use tokio::time::Instant;

/// How long without any network before saying so. A switch from wifi to mobile data loses the
/// old network about half a second before the new one arrives.
const NO_NETWORK_GRACE: Duration = Duration::from_secs(1);
/// How long a relay that was connected may be gone before it shows. It routinely drops and is
/// back within 100–200 ms, and a chip that blinks on every one of those is noise.
const FLICKER: Duration = Duration::from_secs(2);
/// How long to say "connecting" before admitting it is not working. A relay that never answers
/// looked like "connecting" for ten hours once.
const GIVE_UP: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    /// Connected to a home relay: anyone with the key can call.
    Online,
    /// There is a network and no relay yet: starting up, or after the network changed.
    Connecting,
    /// The platform says there is no network at all.
    NoNetwork,
    /// There is a network, and no relay after [`GIVE_UP`].
    Offline,
}

pub struct Reachability {
    relay: bool,
    /// When a connected relay went, while it is still gone. `None` if it never connected, or the
    /// wait started over.
    relay_lost: Option<Instant>,
    /// When the platform said there was no network, while there still is none.
    network_lost: Option<Instant>,
    /// Since when we have been trying: startup, the relay going, or a network coming back.
    waiting: Instant,
}

impl Reachability {
    /// Starts out assuming a network: a platform that cannot tell never says otherwise.
    pub const fn new(now: Instant) -> Self {
        Self { relay: false, relay_lost: None, network_lost: None, waiting: now }
    }

    pub const fn relay_up(&self) -> bool {
        self.relay
    }

    pub const fn relay(&mut self, up: bool, now: Instant) {
        if up {
            self.relay_lost = None;
        } else if self.relay {
            self.relay_lost = Some(now);
            self.waiting = now;
        }
        self.relay = up;
    }

    pub fn network(&mut self, up: bool, now: Instant) {
        if !up {
            self.network_lost.get_or_insert(now);
        } else if let Some(lost) = self.network_lost.take()
            && now >= lost + NO_NETWORK_GRACE
        {
            // Back after "no network" showed: a fresh attempt, and nothing before it is recent. A
            // shorter gap is a switch, which the relay's own flicker allowance already covers.
            self.waiting = now;
            self.relay_lost = None;
        }
    }

    pub fn shown(&self, now: Instant) -> Reach {
        if self.network_lost.is_some_and(|lost| now >= lost + NO_NETWORK_GRACE) {
            return Reach::NoNetwork;
        }
        if self.relay || self.relay_lost.is_some_and(|lost| now < lost + FLICKER) {
            return Reach::Online;
        }
        if now >= self.waiting + GIVE_UP { Reach::Offline } else { Reach::Connecting }
    }

    /// When [`Self::shown`] could next change with no new input, if ever.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        let network = self.network_lost.map(|lost| lost + NO_NETWORK_GRACE);
        let (flicker, give_up) = if self.relay {
            (None, None)
        } else {
            (self.relay_lost.map(|lost| lost + FLICKER), Some(self.waiting + GIVE_UP))
        };
        [network, flicker, give_up].into_iter().flatten().filter(|at| *at > now).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOMENT: Duration = Duration::from_millis(100);
    const SWITCH: Duration = Duration::from_millis(500);

    #[test]
    fn starts_connecting_then_online() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        assert_eq!(reach.shown(start), Reach::Connecting);
        reach.relay(true, start + MOMENT);
        assert_eq!(reach.shown(start + MOMENT), Reach::Online);
        assert_eq!(reach.next_change(start + MOMENT), None);
    }

    #[test]
    fn a_relay_blip_does_not_show() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        reach.relay(true, start);
        reach.relay(false, start + MOMENT);
        assert_eq!(reach.shown(start + MOMENT), Reach::Online);
        assert_eq!(reach.next_change(start + MOMENT), Some(start + MOMENT + FLICKER));
        reach.relay(true, start + MOMENT * 2);
        assert_eq!(reach.shown(start + MOMENT * 2), Reach::Online);
    }

    #[test]
    fn a_relay_gone_for_long_is_connecting_then_offline() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        reach.relay(true, start);
        reach.relay(false, start);
        assert_eq!(reach.shown(start + FLICKER), Reach::Connecting);
        assert_eq!(reach.next_change(start + FLICKER), Some(start + GIVE_UP));
        assert_eq!(reach.shown(start + GIVE_UP), Reach::Offline);
        assert_eq!(reach.next_change(start + GIVE_UP), None);
    }

    #[test]
    fn a_switch_between_networks_is_not_no_network() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        reach.relay(true, start);
        reach.network(false, start);
        reach.relay(false, start + MOMENT);
        reach.network(true, start + SWITCH);
        assert_eq!(reach.shown(start + SWITCH), Reach::Online);
        reach.relay(true, start + SWITCH + MOMENT);
        assert_eq!(reach.shown(start + SWITCH + MOMENT), Reach::Online);
    }

    #[test]
    fn a_switch_whose_relay_does_not_come_back_is_connecting() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        reach.relay(true, start);
        reach.network(false, start);
        reach.relay(false, start);
        reach.network(true, start + SWITCH);
        assert_eq!(reach.next_change(start + SWITCH), Some(start + FLICKER));
        assert_eq!(reach.shown(start + FLICKER), Reach::Connecting);
    }

    #[test]
    fn no_network_shows_after_the_grace_and_clears_when_one_is_back() {
        let start = Instant::now();
        let mut reach = Reachability::new(start);
        reach.relay(true, start);
        reach.network(false, start);
        reach.relay(false, start);
        assert_eq!(reach.shown(start + MOMENT), Reach::Online);
        assert_eq!(reach.shown(start + NO_NETWORK_GRACE), Reach::NoNetwork);
        // Well past the point it would have given up: no network is still the truer answer.
        assert_eq!(reach.shown(start + GIVE_UP), Reach::NoNetwork);
        let back = start + GIVE_UP;
        reach.network(true, back);
        assert_eq!(reach.shown(back), Reach::Connecting);
        assert_eq!(reach.next_change(back), Some(back + GIVE_UP));
    }
}
