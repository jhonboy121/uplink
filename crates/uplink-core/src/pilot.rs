//! Keeps the right relays in iroh's map.
//!
//! iroh probes every relay in its map each sweep, picks the home relay by latency and moves off
//! one that stops answering — all by itself. What it cannot do cheaply is choose *which* relays to
//! probe, because every one in the map costs a handshake every 20–26 s. So the pilot does that:
//! a **survey** puts the whole ticked pool in the map for one sweep, reads each relay's latency
//! from the report that follows, ranks them ([`relays::rank`]) and takes the map back down to the
//! active set ([`relays::active`]). Everything between surveys is iroh's.
//!
//! A survey runs at start, when the network changes, every few hours, when the home relay is
//! lost, and on request — never during a call, where moving the home relay would move the path
//! the other side reaches us by. One asked for mid-call waits for the call to end.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use iroh::unstable_net_report::NetReport;
use iroh::{Endpoint, RelayConfig, RelayUrl, Watcher as _};
use tokio::sync::{mpsc, watch};

use crate::node::Event;
use crate::relays::{self, Choice, Measured, Ranking};
use crate::settings::Settings;

/// Between two surveys when nothing prompts one sooner.
const PERIOD: Duration = Duration::from_secs(6 * 60 * 60);
/// The least time between two surveys, whatever asks: a flapping network should not turn into a
/// sweep of every relay each time it flaps.
const GAP: Duration = Duration::from_secs(60);
/// How long a survey waits for the pool to answer. A relay that has not by then is unreachable
/// from here, which is an answer too.
const WAIT: Duration = Duration::from_secs(15);
/// How long without a home relay counts as having lost it. Moving between relays passes through
/// a moment with none, and that is not a failure.
const LOST: Duration = Duration::from_secs(10);

/// What the UI asks of the pilot.
#[derive(Clone, Copy, Debug)]
pub enum Steer {
    /// The mode or the ticks changed in settings.
    Reload,
    /// Survey now. In manual mode this measures every relay, ticked or not, so the choice can be
    /// made on numbers; the map still ends up as the ticks.
    Check,
}

/// What the relay page shows.
#[derive(Clone, Debug)]
pub struct RelayView {
    pub ranking: Ranking,
    /// What iroh's map holds.
    pub active: Vec<RelayUrl>,
    /// The relay we are reachable through, if any.
    pub home: Option<RelayUrl>,
}

pub(crate) struct Pilot {
    endpoint: Endpoint,
    store: Settings,
    ranking: Ranking,
    last: Option<Instant>,
    /// A survey is owed, and runs as soon as no call is up.
    owed: bool,
    /// Whether this survey is the one "Check now" asked for, which measures everything.
    everything: bool,
    /// Since when no home relay has been connected.
    lost: Option<tokio::time::Instant>,
    /// What iroh's map holds. Kept here because the endpoint does not say.
    map: Vec<RelayUrl>,
    events: mpsc::Sender<Event>,
}

impl Pilot {
    /// `store` must be the one the endpoint was bound with, so that [`Self::initial`] names the
    /// map it was bound with.
    pub(crate) fn new(endpoint: Endpoint, store: Settings, events: mpsc::Sender<Event>) -> Self {
        let (ranking, map) = (Ranking::load(&store), Self::initial(&store));
        Self { endpoint, store, ranking, last: None, owed: true, everything: false, lost: None, map, events }
    }

    /// What iroh's map starts with, before the first survey has run. Never empty.
    pub(crate) fn initial(store: &Settings) -> Vec<RelayUrl> {
        let active = relays::active(&Choice::load(store), &Ranking::load(store), &relays::catalogue(store));
        relays::map(&active, store).urls()
    }

    pub(crate) async fn run(mut self, mut steer: mpsc::Receiver<Steer>, mut in_call: watch::Receiver<bool>) {
        let mut reports = self.endpoint.net_report();
        let mut homes = self.endpoint.home_relay_status();
        let mut seen = addresses(reports.get().as_ref());
        self.show().await;
        loop {
            let next = self.last.map_or_else(tokio::time::Instant::now, |last| (last + PERIOD).into());
            // A survey owed inside the gap after the last one runs when the gap is over, not when
            // something else happens to wake this loop.
            let gap_over = self.last.map_or_else(tokio::time::Instant::now, |last| (last + GAP).into());
            let waiting = self.owed && gap_over > tokio::time::Instant::now();
            tokio::select! {
                () = tokio::time::sleep_until(gap_over), if waiting => {}
                command = steer.recv() => match command {
                    Some(Steer::Reload) => {
                        let choice = Choice::load(&self.store);
                        // Manual is exactly the ticks, so there is nothing to measure first.
                        if choice.auto { self.owe("choice changed") } else { self.apply(&choice).await }
                    }
                    Some(Steer::Check) => {
                        self.everything = true;
                        self.last = None;
                        self.owe("asked");
                    }
                    None => break,
                },
                () = tokio::time::sleep_until(next), if self.last.is_some() => self.owe("periodic"),
                changed = reports.updated() => match changed {
                    Ok(report) => {
                        let now = addresses(report.as_ref());
                        // A report with no public address says nothing about a new network: it is
                        // what having none at all looks like, and a survey would find nothing.
                        if now != seen && now != (None, None) {
                            tracing::info!(before = ?seen, after = ?now, "relay pilot: network changed");
                            self.owe("network changed");
                        }
                        seen = now;
                    }
                    Err(_) => break,
                },
                changed = homes.updated() => match changed {
                    Ok(statuses) => {
                        let connected = statuses.iter().any(|status| status.is_connected());
                        self.lost = match (connected, self.lost) {
                            (true, _) => None,
                            (false, None) => Some(tokio::time::Instant::now()),
                            (false, since) => since,
                        };
                        self.show().await;
                    }
                    Err(_) => break,
                },
                () = tokio::time::sleep_until(self.lost.map_or_else(tokio::time::Instant::now, |since| since + LOST)),
                    if self.lost.is_some() => {
                    self.lost = None;
                    self.owe("home relay lost");
                }
                changed = in_call.changed() => if changed.is_err() { break },
            }
            if self.owed && !*in_call.borrow() && self.last.is_none_or(|last| last.elapsed() >= GAP) {
                self.owed = false;
                self.survey().await;
                self.everything = false;
            }
        }
        tracing::debug!("relay pilot stopped");
    }

    fn owe(&mut self, why: &'static str) {
        tracing::debug!(why, "relay pilot: survey owed");
        self.owed = true;
    }

    async fn survey(&mut self) {
        let choice = Choice::load(&self.store);
        let catalogue = relays::catalogue(&self.store);
        if !choice.auto && !self.everything {
            return self.apply(&choice).await;
        }
        let pool: Vec<RelayUrl> = if self.everything {
            catalogue.iter().map(|relay| relay.url.clone()).collect()
        } else {
            choice.pool(&catalogue)
        };
        self.last = Some(Instant::now());
        let mut reports = self.endpoint.net_report();
        self.set(&pool).await;
        let mut best: Vec<Measured> = Vec::new();
        let waited = tokio::time::timeout(WAIT, async {
            while let Ok(report) = reports.updated().await {
                if let Some(report) = report {
                    merge(&mut best, &report);
                }
                if pool.iter().all(|url| best.iter().any(|relay| &relay.url == url)) {
                    break;
                }
            }
        })
        .await;
        for url in &pool {
            match best.iter().find(|relay| &relay.url == url) {
                Some(relay) => tracing::info!(%url, rtt_ms = relay.rtt.as_millis(), "relay survey"),
                None => tracing::info!(%url, "relay survey: no answer"),
            }
        }
        if best.is_empty() {
            // Nothing answered: no network, most likely. The last ranking is the better guess.
            tracing::warn!(timed_out = waited.is_err(), "relay survey: nothing answered; keeping the last ranking");
        } else {
            self.ranking = Ranking { at: Some(SystemTime::now()), relays: relays::rank(best) };
            if let Err(e) = self.ranking.save(&self.store).await {
                tracing::warn!("storing the relay ranking: {e}");
            }
        }
        self.apply(&choice).await;
    }

    async fn apply(&mut self, choice: &Choice) {
        let active = relays::active(choice, &self.ranking, &relays::catalogue(&self.store));
        self.set(&active).await;
        tracing::info!(auto = choice.auto, active = ?active.iter().map(ToString::to_string).collect::<Vec<_>>(), "relays applied");
        self.show().await;
    }

    /// Makes iroh's map exactly `want`, adding before removing so it is never empty on the way.
    async fn set(&mut self, want: &[RelayUrl]) {
        let want: Vec<RelayUrl> = relays::map(want, &self.store).urls();
        for url in want.iter().filter(|url| !self.map.contains(url)) {
            self.endpoint.insert_relay(url.clone(), Arc::new(RelayConfig::from(url.clone()))).await;
        }
        for url in self.map.iter().filter(|url| !want.contains(url)) {
            self.endpoint.remove_relay(url).await;
        }
        self.map = want;
    }

    async fn show(&self) {
        let home = self.endpoint.home_relay_status().get().into_iter().find(|status| status.is_connected());
        let view = RelayView {
            ranking: self.ranking.clone(),
            active: self.map.clone(),
            home: home.map(|status| status.url().clone()),
        };
        if self.events.send(Event::Relays(view)).await.is_err() {
            tracing::debug!("relay view dropped: no listener");
        }
    }
}

/// The public addresses a report found, which is what changes when the phone changes network.
/// Addresses, not ports: a NAT may hand out a new port every sweep without anything moving.
fn addresses(report: Option<&NetReport>) -> (Option<Ipv4Addr>, Option<Ipv6Addr>) {
    report.map_or((None, None), |report| {
        (report.global_v4.map(|addr| *addr.ip()), report.global_v6.map(|addr| *addr.ip()))
    })
}

/// Keeps each relay's best latency across the reports a survey sees, whichever probe found it.
fn merge(best: &mut Vec<Measured>, report: &NetReport) {
    for (_, url, rtt) in report.relay_latency.iter() {
        match best.iter_mut().find(|relay| &relay.url == url) {
            Some(relay) => relay.rtt = relay.rtt.min(rtt),
            None => best.push(Measured { url: url.clone(), rtt }),
        }
    }
}
