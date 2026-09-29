//! Which relays the endpoint talks to.
//!
//! One catalogue — uplink's own, n0's, and any you add — and a tick against each: a tick means
//! "may be used". Stored as the ticked set, never as the unticked one, so what is stored is what
//! is used; the price is that a relay n0 publishes later arrives unticked.
//!
//! Automatic (the default) narrows the ticked set to two: the best one, and a standby iroh keeps
//! probing so its own failover has somewhere to go. Manual uses every ticked relay. The idle cost
//! is per relay in iroh's map — each is probed every 20–26 s for the life of the process — which
//! is why automatic is two and not five. See [`crate::pilot`] for what keeps the two current.

use std::str::FromStr as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::{RelayMap, RelayUrl, defaults::prod};

use crate::Error;
use crate::settings::{RELAYS_CUSTOM, RELAYS_MANUAL, RELAYS_RANKING, RELAYS_TICKED, Settings};

/// uplink's own relay, self-hosted. Preferred: see [`rank`]. Set at build time by `UPLINK_RELAY`,
/// from the uncommitted `.cargo/config.local.toml`; an example domain without it.
pub const UPLINK: &str = match option_env!("UPLINK_RELAY") {
    Some(url) => url,
    None => "https://uplink-relay.example.com",
};

/// How many relays automatic keeps in iroh's map: one in use and one standby.
const ACTIVE: usize = 2;

/// Another relay outranks uplink's only when it is faster than this share of uplink's latency —
/// the same margin iroh uses before it moves the home relay, so the two never disagree.
const PREFER_NUM: u32 = 2;
const PREFER_DEN: u32 = 3;

/// Whose relay it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Uplink,
    N0,
    Yours,
}

/// Where a relay is. The words are the UI's, in whichever language it is showing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    India,
    NorthAmericaEast,
    NorthAmericaWest,
    Europe,
    AsiaPacific,
    /// One of n0's whose label this does not know: a relay added since this was written.
    Elsewhere,
    /// Added by the user, and called by the name they gave it.
    Yours,
}

impl Region {
    /// Where n0 puts a relay, read from the label it names it with.
    fn of_n0(label: &str) -> Self {
        match label.get(..3) {
            Some("use") => Self::NorthAmericaEast,
            Some("usw") => Self::NorthAmericaWest,
            Some("euc") => Self::Europe,
            Some("aps") => Self::AsiaPacific,
            _ => Self::Elsewhere,
        }
    }
}

/// One relay in the catalogue.
#[derive(Clone, Debug)]
pub struct Relay {
    pub url: RelayUrl,
    /// The name it was added with, or the first label of its host.
    pub name: String,
    pub source: Source,
    pub region: Region,
}

impl Relay {
    pub fn host(&self) -> &str {
        self.url.host_str().unwrap_or_default()
    }
}

/// A relay the user added: where it is, and what they call it.
#[derive(Clone, Debug)]
pub struct Custom {
    pub url: RelayUrl,
    pub name: String,
}

impl Custom {
    /// `<url> <name>`, split at the first space — a URL has none and a name may have several.
    fn parse(line: &str) -> Option<Self> {
        let (url, name) = line.split_once(' ').unwrap_or((line, ""));
        match RelayUrl::from_str(url) {
            Ok(parsed) => Some(Self { url: parsed, name: if name.is_empty() { url } else { name }.to_owned() }),
            // A bad row is skipped rather than fatal: this is a preference, and one unparseable
            // line should not be the reason a phone cannot be called.
            Err(e) => {
                tracing::warn!(url, "stored relay: {e}");
                None
            }
        }
    }

    pub fn line(&self) -> String {
        format!("{} {}", self.url, self.name)
    }
}

fn uplink() -> Option<RelayUrl> {
    RelayUrl::from_str(UPLINK).inspect_err(|e| tracing::error!("uplink's relay: {e}")).ok()
}

/// Every relay there is to choose from: uplink's, then n0's in their map's order, then yours.
pub fn catalogue(store: &Settings) -> Vec<Relay> {
    let ours = uplink().map(|url| Relay { url, name: String::new(), source: Source::Uplink, region: Region::India });
    let n0 = prod::default_relay_map().urls::<Vec<_>>().into_iter().map(|url| {
        let label = url.host_str().and_then(|host| host.split('.').next()).unwrap_or_default().to_owned();
        Relay { region: Region::of_n0(&label), name: label, source: Source::N0, url }
    });
    let yours = custom(store).into_iter().map(|relay| Relay {
        url: relay.url,
        name: relay.name,
        source: Source::Yours,
        region: Region::Yours,
    });
    ours.into_iter().chain(n0).chain(yours).collect()
}

/// The mode and the ticks, as settings hold them.
#[derive(Clone, Debug)]
pub struct Choice {
    pub auto: bool,
    /// Never stored empty by the UI; an empty list read back is treated as unset.
    pub ticked: Vec<RelayUrl>,
}

impl Choice {
    /// Absent ticks mean all of them, so a fresh install lets automatic choose from everything.
    pub fn load(store: &Settings) -> Self {
        let ticked: Vec<RelayUrl> =
            store.lines(RELAYS_TICKED).iter().filter_map(|line| RelayUrl::from_str(line).ok()).collect();
        let ticked =
            if ticked.is_empty() { catalogue(store).into_iter().map(|relay| relay.url).collect() } else { ticked };
        Self { auto: !store.flag(RELAYS_MANUAL), ticked }
    }

    pub async fn save(&self, store: &Settings) -> Result<(), Error> {
        store.set_flag(RELAYS_MANUAL, !self.auto).await?;
        let lines: Vec<String> = self.ticked.iter().map(ToString::to_string).collect();
        store.set_lines(RELAYS_TICKED, &lines).await
    }

    /// The ticked relays that still exist, in catalogue order: a tick against a removed relay of
    /// yours is not something to hand iroh.
    pub fn pool(&self, catalogue: &[Relay]) -> Vec<RelayUrl> {
        catalogue
            .iter()
            .filter(|relay| self.ticked.contains(&relay.url))
            .map(|relay| relay.url.clone())
            .collect()
    }
}

/// One relay's round trip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measured {
    pub url: RelayUrl,
    pub rtt: Duration,
}

/// The last survey: every relay that answered, best first, and when.
#[derive(Clone, Debug, Default)]
pub struct Ranking {
    pub at: Option<SystemTime>,
    pub relays: Vec<Measured>,
}

impl Ranking {
    /// Stored as the survey's unix time on the first line, then `<url> <rtt ms>` in rank order.
    pub fn load(store: &Settings) -> Self {
        let lines = store.lines(RELAYS_RANKING);
        let Some((first, rest)) = lines.split_first() else {
            return Self::default();
        };
        let at = first.parse().ok().map(|secs| UNIX_EPOCH + Duration::from_secs(secs));
        let relays = rest
            .iter()
            .filter_map(|line| {
                let (url, ms) = line.split_once(' ')?;
                Some(Measured { url: RelayUrl::from_str(url).ok()?, rtt: Duration::from_millis(ms.parse().ok()?) })
            })
            .collect();
        Self { at, relays }
    }

    pub async fn save(&self, store: &Settings) -> Result<(), Error> {
        let at = self.at.and_then(|at| at.duration_since(UNIX_EPOCH).ok()).unwrap_or_default().as_secs();
        let lines: Vec<String> = std::iter::once(at.to_string())
            .chain(self.relays.iter().map(|relay| format!("{} {}", relay.url, relay.rtt.as_millis())))
            .collect();
        store.set_lines(RELAYS_RANKING, &lines).await
    }

    pub fn rtt(&self, url: &RelayUrl) -> Option<Duration> {
        self.relays.iter().find(|relay| &relay.url == url).map(|relay| relay.rtt)
    }
}

/// Best first, by latency, except that uplink's own relay stays ahead of any that is not clearly
/// faster. Ties keep uplink's first.
pub fn rank(mut measured: Vec<Measured>) -> Vec<Measured> {
    let ours = uplink();
    let key = |relay: &Measured| {
        let preferred = Some(&relay.url) == ours.as_ref();
        let rtt = if preferred { relay.rtt * PREFER_NUM / PREFER_DEN } else { relay.rtt };
        (rtt, !preferred)
    };
    measured.sort_by_key(key);
    measured
}

/// What iroh's map should hold.
///
/// Manual: every ticked relay. Automatic: the best ticked one, and as standby the best of the rest
/// that is **not faster than it** — iroh picks the home relay by latency alone, so a faster standby
/// would win the pick and undo uplink's preference. With nothing slower, the best one goes alone
/// and [`crate::pilot`] fails over for it. With nothing measured, the whole pool, which is itself
/// the survey that will narrow it.
pub fn active(choice: &Choice, ranking: &Ranking, catalogue: &[Relay]) -> Vec<RelayUrl> {
    let pool = choice.pool(catalogue);
    if !choice.auto {
        return pool;
    }
    let mut ranked = ranking.relays.iter().filter(|relay| pool.contains(&relay.url));
    let Some(best) = ranked.next() else {
        return pool;
    };
    let standby = ranked.find(|relay| relay.rtt >= best.rtt);
    std::iter::once(best).chain(standby).take(ACTIVE).map(|relay| relay.url.clone()).collect()
}

/// The map to hand iroh. Never empty: a phone with no relay cannot be called at all, so an empty
/// choice falls back to the whole catalogue rather than leaving it unreachable quietly.
pub fn map(urls: &[RelayUrl], store: &Settings) -> RelayMap {
    if urls.is_empty() {
        tracing::warn!("no relay is ticked; using them all");
        return RelayMap::from_iter(catalogue(store).into_iter().map(|relay| relay.url));
    }
    RelayMap::from_iter(urls.iter().cloned())
}

/// The relays yours, whether or not they are ticked.
pub fn custom(store: &Settings) -> Vec<Custom> {
    store.lines(RELAYS_CUSTOM).iter().filter_map(|line| Custom::parse(line)).collect()
}

/// Adds a relay of yours, ticked. The URL is parsed here rather than stored and discovered to be
/// unusable later, so a typo is refused while the person who made it is still looking at it.
pub async fn add_custom(store: &Settings, name: &str, url: &str) -> Result<(), Error> {
    let url = RelayUrl::from_str(url.trim()).map_err(|_| Error::RelayUrl(url.to_owned()))?;
    let name = name.trim();
    let added = Custom { name: if name.is_empty() { url.to_string() } else { name.to_owned() }, url };
    // Read before the list changes: absent ticks mean all, and they must go on meaning all of the
    // old catalogue plus this one, not be pinned to it alone.
    let mut choice = Choice::load(store);
    let mut lines: Vec<String> = custom(store)
        .into_iter()
        .filter(|stored| stored.url != added.url)
        .map(|stored| stored.line())
        .collect();
    lines.push(added.line());
    store.set_lines(RELAYS_CUSTOM, &lines).await?;
    if !choice.ticked.contains(&added.url) {
        choice.ticked.push(added.url);
    }
    choice.save(store).await
}

/// Removes one of yours, and its tick with it.
pub async fn remove_custom(store: &Settings, url: &RelayUrl) -> Result<(), Error> {
    let mut choice = Choice::load(store);
    let lines: Vec<String> =
        custom(store).into_iter().filter(|stored| &stored.url != url).map(|stored| stored.line()).collect();
    store.set_lines(RELAYS_CUSTOM, &lines).await?;
    choice.ticked.retain(|ticked| ticked != url);
    choice.save(store).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    async fn settings() -> Result<Settings, Error> {
        Settings::open(Db::memory().await?).await
    }

    fn url(s: &str) -> Result<RelayUrl, Error> {
        RelayUrl::from_str(s).map_err(|_| Error::RelayUrl(s.to_owned()))
    }

    fn measured(s: &str, ms: u64) -> Result<Measured, Error> {
        Ok(Measured { url: url(s)?, rtt: Duration::from_millis(ms) })
    }

    #[tokio::test]
    async fn a_fresh_install_ticks_everything_and_is_automatic() -> Result<(), Error> {
        let store = settings().await?;
        let choice = Choice::load(&store);
        assert!(choice.auto);
        assert_eq!(choice.ticked.len(), catalogue(&store).len());
        Ok(())
    }

    /// uplink's relay keeps first place against one that is only a little faster, and loses it to
    /// one under two thirds of its latency.
    #[test]
    fn uplink_is_preferred_unless_clearly_slower() -> Result<(), Error> {
        let close = rank(vec![measured("https://a.example.com", 50)?, measured(UPLINK, 60)?]);
        assert_eq!(close[0].url, url(UPLINK)?);
        let far = rank(vec![measured("https://a.example.com", 30)?, measured(UPLINK, 60)?]);
        assert_eq!(far[0].url, url("https://a.example.com")?);
        Ok(())
    }

    /// The standby must not be faster than the one in use, or iroh would pick it instead.
    #[tokio::test]
    async fn the_standby_is_never_faster_than_the_primary() -> Result<(), Error> {
        let store = settings().await?;
        let choice = Choice { auto: true, ticked: catalogue(&store).into_iter().map(|relay| relay.url).collect() };
        let [aps, euc] = [prod::default_ap_relay().url, prod::default_eu_relay().url];
        let ranking = Ranking {
            at: None,
            relays: rank(vec![
                measured(UPLINK, 60)?,
                Measured { url: aps.clone(), rtt: Duration::from_millis(50) },
                Measured { url: euc.clone(), rtt: Duration::from_millis(140) },
            ]),
        };
        assert_eq!(active(&choice, &ranking, &catalogue(&store)), vec![url(UPLINK)?, euc]);
        Ok(())
    }

    #[tokio::test]
    async fn manual_uses_every_ticked_relay() -> Result<(), Error> {
        let store = settings().await?;
        let ticked = vec![url(UPLINK)?, prod::default_ap_relay().url];
        let choice = Choice { auto: false, ticked: ticked.clone() };
        assert_eq!(active(&choice, &Ranking::default(), &catalogue(&store)), ticked);
        Ok(())
    }

    #[tokio::test]
    async fn a_ranking_round_trips() -> Result<(), Error> {
        let store = settings().await?;
        let ranking =
            Ranking { at: Some(UNIX_EPOCH + Duration::from_secs(1_000)), relays: vec![measured(UPLINK, 38)?] };
        ranking.save(&store).await?;
        let back = Ranking::load(&store);
        assert_eq!(back.at, ranking.at);
        assert_eq!(back.relays, ranking.relays);
        Ok(())
    }

    /// Adding is by URL, so the same relay twice is one relay with the newer name, and it is ticked.
    #[tokio::test]
    async fn an_added_relay_is_ticked_once() -> Result<(), Error> {
        let store = settings().await?;
        add_custom(&store, "Home", "https://relay.example.com").await?;
        add_custom(&store, "The one at home", "https://relay.example.com").await?;
        let listed = custom(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "The one at home");
        let choice = Choice::load(&store);
        assert_eq!(choice.ticked.iter().filter(|ticked| **ticked == listed[0].url).count(), 1);
        assert_eq!(choice.ticked.len(), catalogue(&store).len());
        Ok(())
    }

    #[tokio::test]
    async fn a_removed_relay_loses_its_tick() -> Result<(), Error> {
        let store = settings().await?;
        add_custom(&store, "Home", "https://relay.example.com").await?;
        remove_custom(&store, &url("https://relay.example.com")?).await?;
        assert!(custom(&store).is_empty());
        assert!(!Choice::load(&store).ticked.contains(&url("https://relay.example.com")?));
        Ok(())
    }

    /// A typo is refused while it is still on screen, rather than stored and found unusable later.
    #[tokio::test]
    async fn a_relay_that_is_not_a_url_is_refused() -> Result<(), Error> {
        let store = settings().await?;
        assert!(add_custom(&store, "Nope", "not a url").await.is_err());
        assert!(custom(&store).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn the_map_is_never_empty() -> Result<(), Error> {
        let store = settings().await?;
        assert!(!map(&[], &store).is_empty());
        Ok(())
    }
}
