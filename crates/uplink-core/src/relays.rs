//! Which relays the endpoint talks to.
//!
//! Two shapes, because the two answer different questions. n0 publishes a set and changes it over
//! time, so what is stored against it is the *subtraction* — the hosts switched off — and anything
//! n0 adds later is on. A stored list of the ones that are on would have got that backwards, and
//! silently: a relay added next year would be off for everyone who had ever opened this screen.
//! A custom set has no publisher behind it, so there the stored list is the whole map, names and
//! all, because a bare URL is not something anyone recognises a month later.

use std::str::FromStr as _;

use iroh::{RelayMap, RelayUrl, defaults::prod};

use crate::Error;
use crate::settings::{RELAY_SOURCE, RELAYS_CUSTOM, RELAYS_OFF, Settings};

/// The stored value that means "not n0's". Anything else, including nothing, means n0's.
const SOURCE_CUSTOM: &str = "custom";

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
            Ok(parsed) => {
                Some(Self { url: parsed, name: if name.is_empty() { url } else { name }.to_owned() })
            }
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

/// Where the relay map comes from.
#[derive(Clone, Debug)]
pub enum Relays {
    /// Everything n0 publishes, minus these hosts.
    N0 { off: Vec<String> },
    /// Only these. Nothing of n0's is used, so this list is the whole map.
    Custom(Vec<Custom>),
}

/// One row of the relay list, as the settings screen shows it.
pub struct Relay {
    /// What the off-list names, and what tells one relay from another.
    pub host: String,
    /// What to call it on screen: n0's own label for the location, or the name it was added with.
    pub name: String,
    pub region: String,
    pub on: bool,
}

impl Relays {
    /// What settings say. An unreadable or absent choice is all of n0's, because a relay list is
    /// a preference and failing to read one should not leave a phone unreachable.
    pub fn load(store: &Settings) -> Self {
        if store.get(RELAY_SOURCE).as_deref() == Some(SOURCE_CUSTOM) {
            let lines = store.lines(RELAYS_CUSTOM);
            return Self::Custom(lines.iter().filter_map(|line| Custom::parse(line)).collect());
        }
        Self::N0 { off: store.lines(RELAYS_OFF) }
    }

    /// The map to hand iroh. Never empty: a phone with no relay cannot be called at all, and
    /// neither an off-list nor an emptied custom list should be able to do that quietly.
    pub fn map(&self) -> RelayMap {
        let map = match self {
            Self::N0 { off } => RelayMap::from_iter(
                prod::default_relay_map()
                    .relays::<Vec<_>>()
                    .into_iter()
                    .filter(|relay| !off.iter().any(|host| Some(host.as_str()) == relay.url.host_str())),
            ),
            Self::Custom(relays) => RelayMap::from_iter(relays.iter().map(|relay| relay.url.clone())),
        };
        if map.is_empty() {
            tracing::warn!("no relay is switched on; using n0's");
            return prod::default_relay_map();
        }
        map
    }

    /// Every relay this source knows about, and whether it is on. A custom relay is on by being
    /// in the list, which is why there is nothing to switch off — only to remove.
    pub fn listed(&self) -> Vec<Relay> {
        match self {
            Self::N0 { off } => published()
                .into_iter()
                .map(|relay| Relay { on: !off.contains(&relay.host), ..relay })
                .collect(),
            Self::Custom(relays) => relays
                .iter()
                .map(|relay| Relay {
                    host: relay.url.host_str().unwrap_or_default().to_owned(),
                    name: relay.name.clone(),
                    region: "Added by you".to_owned(),
                    on: true,
                })
                .collect(),
        }
    }
}

/// n0's own relays, in the map's order, all on. [`Relays::listed`] is what applies the off-list.
fn published() -> Vec<Relay> {
    let urls = prod::default_relay_map().urls::<Vec<_>>();
    urls.iter()
        .filter_map(|url| url.host_str())
        .map(|host| {
            let name = host.split('.').next().unwrap_or(host);
            Relay { host: host.to_owned(), name: name.to_owned(), region: region(name).to_owned(), on: true }
        })
        .collect()
}

/// Where n0 puts a relay, read from the label it names it with. An unrecognised label is a relay
/// added since this was written — exactly the case the off-list exists to get right, so it is
/// listed, it is on, and it is named as plainly as we can manage.
fn region(name: &str) -> &'static str {
    match name.get(..3) {
        Some("use") => "North America, east",
        Some("usw") => "North America, west",
        Some("euc") => "Europe",
        Some("aps") => "Asia-Pacific",
        _ => "n0 relay",
    }
}

/// Switches one of n0's relays on or off. The off-list is the only thing written: the set itself
/// is n0's to change.
pub fn set_off(store: &Settings, host: &str, off: bool) -> Result<(), Error> {
    let mut hosts = store.lines(RELAYS_OFF);
    hosts.retain(|stored| stored != host);
    if off {
        hosts.push(host.to_owned());
    }
    store.set_lines(RELAYS_OFF, &hosts)
}

/// Whether the custom set is the one in use. Read on its own because the screen shows both lists
/// whichever is live — you pick a relay before you switch to it, not after.
pub fn uses_custom(store: &Settings) -> bool {
    store.get(RELAY_SOURCE).as_deref() == Some(SOURCE_CUSTOM)
}

pub fn set_uses_custom(store: &Settings, custom: bool) -> Result<(), Error> {
    store.set(RELAY_SOURCE, if custom { SOURCE_CUSTOM } else { "n0" })
}

/// The custom set, whether or not it is the one in use.
pub fn custom(store: &Settings) -> Vec<Custom> {
    store.lines(RELAYS_CUSTOM).iter().filter_map(|line| Custom::parse(line)).collect()
}

/// Adds a relay to the custom set. The URL is parsed here rather than stored and discovered to be
/// unusable later, so a typo is refused while the person who made it is still looking at it.
pub fn add_custom(store: &Settings, name: &str, url: &str) -> Result<(), Error> {
    let url = RelayUrl::from_str(url.trim()).map_err(|_| Error::RelayUrl(url.to_owned()))?;
    let name = name.trim();
    let added = Custom { name: if name.is_empty() { url.to_string() } else { name.to_owned() }, url };
    let mut lines: Vec<String> = custom(store)
        .into_iter()
        .filter(|stored| stored.url != added.url)
        .map(|stored| stored.line())
        .collect();
    lines.push(added.line());
    store.set_lines(RELAYS_CUSTOM, &lines)
}

/// Removes one by host, which is what the row carries.
pub fn remove_custom(store: &Settings, host: &str) -> Result<(), Error> {
    let lines: Vec<String> = custom(store)
        .into_iter()
        .filter(|stored| stored.url.host_str() != Some(host))
        .map(|stored| stored.line())
        .collect();
    store.set_lines(RELAYS_CUSTOM, &lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn settings() -> Result<Settings, Error> {
        Settings::open(Db::memory()?)
    }

    /// The whole point of storing the off-list rather than the on-list: a relay n0 adds later is
    /// on, without anyone having to touch their settings again.
    #[test]
    fn an_unknown_relay_is_on() -> Result<(), Error> {
        let store = settings()?;
        let first = published().first().map(|relay| relay.host.clone()).unwrap_or_default();
        set_off(&store, &first, true)?;
        let listed = Relays::load(&store).listed();
        assert!(listed.iter().any(|relay| relay.host == first && !relay.on));
        assert!(listed.iter().filter(|relay| relay.host != first).all(|relay| relay.on));
        Ok(())
    }

    /// Switching every relay off would leave the phone uncallable, so the map refuses to be empty.
    #[test]
    fn the_map_is_never_empty() -> Result<(), Error> {
        let store = settings()?;
        for relay in published() {
            set_off(&store, &relay.host, true)?;
        }
        assert!(!Relays::load(&store).map().is_empty());
        Ok(())
    }

    /// Adding is by URL, so the same relay twice is one relay with the newer name.
    #[test]
    fn adding_the_same_relay_twice_renames_it() -> Result<(), Error> {
        let store = settings()?;
        add_custom(&store, "Home", "https://relay.example.com")?;
        add_custom(&store, "The one at home", "https://relay.example.com")?;
        let listed = custom(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "The one at home");
        Ok(())
    }

    /// A typo is refused while it is still on screen, rather than stored and found unusable at
    /// the next bind.
    #[test]
    fn a_relay_that_is_not_a_url_is_refused() -> Result<(), Error> {
        let store = settings()?;
        assert!(add_custom(&store, "Nope", "not a url").is_err());
        assert!(custom(&store).is_empty());
        Ok(())
    }

    #[test]
    fn a_removed_relay_stays_removed() -> Result<(), Error> {
        let store = settings()?;
        add_custom(&store, "Home", "https://relay.example.com")?;
        add_custom(&store, "Work", "https://other.example.com")?;
        remove_custom(&store, "relay.example.com")?;
        let listed = custom(&store);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Work");
        Ok(())
    }

    /// Emptying the custom set while it is the one in use must not leave the phone uncallable.
    #[test]
    fn an_empty_custom_set_falls_back() -> Result<(), Error> {
        let store = settings()?;
        set_uses_custom(&store, true)?;
        assert!(!Relays::load(&store).map().is_empty());
        Ok(())
    }

    /// A custom relay keeps the name it was given, and a nameless one falls back to its URL.
    #[test]
    fn a_custom_relay_round_trips() -> Result<(), Error> {
        let store = settings()?;
        let named = Custom::parse("https://relay.example.com Home").ok_or(Error::NodeStopped)?;
        store.set(RELAY_SOURCE, SOURCE_CUSTOM)?;
        store.set_lines(RELAYS_CUSTOM, &[named.line()])?;
        match Relays::load(&store) {
            Relays::Custom(relays) => {
                assert_eq!(relays.len(), 1);
                assert_eq!(relays[0].name, "Home");
            }
            Relays::N0 { .. } => panic!("stored a custom set and read back n0's"),
        }
        Ok(())
    }
}
