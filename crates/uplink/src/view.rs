//! What the screens are shown: the core's records turned into the markup's types. Facts only —
//! enums, counts, names and figures. The words around them are the markup's, so every sentence
//! on screen is one the bundled translations can reach.

use std::time::{Duration, SystemTime};

use rustc_hash::FxHashSet;
use slint::{ComponentHandle as _, Model as _, ModelRc, SharedString, VecModel};
use uplink_android::platform::{Permission as AndroidPermission, Route as AndroidRoute, RouteKind};
use uplink_core::EndpointId;
use uplink_core::calls::{CallId, Logged, Outcome};
use uplink_core::contacts::{Contact, Contacts};
use uplink_core::health::Weak as CoreWeak;
use uplink_core::media::Route as MediaRoute;
use uplink_core::node::{Mode, RelayView};
use uplink_core::preset::Preset as CorePreset;
use uplink_core::quality::{Quality, Spread};
use uplink_core::reach::Reach as CoreReach;
use uplink_core::relays::{self, Choice, Ranking, Region as RelayRegion};

use crate::clock::LocalClock;
use crate::ui::{
    Ago, App, CallDetail, CallItem, ContactDetail, ContactItem, Day, DayAgo, Ending, Group, Ipv6, Measure, Output,
    OutputItem, Path, PathKind, Permission, Preset, PresetItem, Reach, Region, RelayItem, RelayUse, Route, Say, Stall,
    Stat, Theme, Toast, Traffic, Unit, Weak,
};

/// Groups of four, the way the key is read aloud, over two even lines.
const FINGERPRINT_GROUP: usize = 4;
const FINGERPRINT_GROUPS: usize = 8;
const FINGERPRINT_LINE_GROUPS: usize = 4;
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_HOUR: u64 = 60;
const HOURS_PER_DAY: u64 = 24;
const DAYS_PER_WEEK: u64 = 7;
const PERCENT: f64 = 100.0;
const KBPS_PER_MBPS: f64 = 1000.0;
/// Decimal places a figure is shown with.
const WHOLE: usize = 0;
const ONE_PLACE: usize = 1;

/// An optional value as the markup takes one: a list of at most one.
pub fn maybe<T: Clone + 'static>(value: Option<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from_iter(value))
}

pub fn one<T: Clone + 'static>(value: T) -> ModelRc<T> {
    maybe(Some(value))
}

pub fn none<T: Clone + 'static>() -> ModelRc<T> {
    maybe(None)
}

pub fn list<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// The letter an avatar shows.
pub fn initial(name: &str) -> SharedString {
    name.chars().next().unwrap_or('?').to_uppercase().to_string().into()
}

/// The key's leading groups, which is what anyone compares out loud.
pub fn fingerprint_lines(id: &EndpointId) -> ModelRc<SharedString> {
    let key = id.to_string();
    let lines: Vec<SharedString> = key
        .chars()
        .take(FINGERPRINT_GROUP * FINGERPRINT_GROUPS)
        .collect::<Vec<_>>()
        .chunks(FINGERPRINT_GROUP * FINGERPRINT_LINE_GROUPS)
        .map(|line| {
            line.chunks(FINGERPRINT_GROUP)
                .map(|group| group.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join(" ")
                .into()
        })
        .collect();
    list(lines)
}

pub fn short(id: &EndpointId) -> String {
    id.fmt_short().to_string()
}

/// What you call them, else the start of their key.
pub fn name_of(contacts: &Contacts, id: &EndpointId) -> String {
    contacts.name_of(id).map_or_else(|| short(id), str::to_owned)
}

/// Contacts for People. The store already orders favourites first, so a group starts wherever
/// the flag changes.
pub fn contact_items(
    contacts: &Contacts,
    selected: &FxHashSet<EndpointId>,
    fresh: Option<EndpointId>,
) -> Vec<ContactItem> {
    let mut previous: Option<bool> = None;
    contacts
        .iter()
        .map(|contact| {
            let heading = (previous != Some(contact.favourite)).then_some(if contact.favourite {
                Group::Favourites
            } else {
                Group::Others
            });
            previous = Some(contact.favourite);
            ContactItem {
                name: contact.name.as_str().into(),
                id: contact.id.to_string().into(),
                initial: initial(&contact.name),
                tint: 0,
                heading: maybe(heading),
                last_called: maybe(contact.last_called.map(ago)),
                favourite: contact.favourite,
                selected: selected.contains(&contact.id),
                fresh: fresh == Some(contact.id),
            }
        })
        .collect()
}

/// Where the fresh row's top sits in the People list, in logical pixels, summed the way the
/// markup stacks it: a heading above the first row of a group, a hairline above any other row.
pub fn fresh_offset(ui: &App, items: &[ContactItem]) -> Option<f32> {
    let theme = ui.global::<Theme>();
    let (row, heading, hairline) = (theme.get_row_height(), theme.get_group_head(), theme.get_hairline_width());
    let mut y = 0.0;
    for (index, item) in items.iter().enumerate() {
        y += if item.heading.row_count() > 0 {
            heading
        } else if index > 0 {
            hairline
        } else {
            0.0
        };
        if item.fresh {
            return Some(y);
        }
        y += row;
    }
    None
}

pub fn contact_detail(contact: &Contact) -> ContactDetail {
    ContactDetail {
        id: contact.id.to_string().into(),
        name: contact.name.as_str().into(),
        initial: initial(&contact.name),
        advertised: maybe(contact.advertised.as_deref().map(SharedString::from)),
        favourite: contact.favourite,
        fingerprint: fingerprint_lines(&contact.id),
    }
}

/// Coarse on purpose: the second line of a contact row answers "recently?", not "when exactly?".
/// A time in the future — a clock set back — is "just now".
fn ago(at: SystemTime) -> Ago {
    let minutes = SystemTime::now().duration_since(at).map_or(0, |ago| ago.as_secs() / SECONDS_PER_MINUTE);
    let hours = minutes / MINUTES_PER_HOUR;
    let days = hours / HOURS_PER_DAY;
    let (unit, count) = if minutes < 1 {
        (Unit::Now, 0)
    } else if hours < 1 {
        (Unit::Minutes, minutes)
    } else if days < 1 {
        (Unit::Hours, hours)
    } else if days < DAYS_PER_WEEK {
        (Unit::Days, days)
    } else {
        (Unit::Weeks, days / DAYS_PER_WEEK)
    };
    Ago { unit, count: count_of(count) }
}

fn count_of(count: impl TryInto<i32>) -> i32 {
    count.try_into().unwrap_or(i32::MAX)
}

/// Calendar days back from today, as the log's day headings count them.
fn day_ago(days: i64) -> DayAgo {
    let week = i64::try_from(DAYS_PER_WEEK).unwrap_or(i64::MAX);
    let (day, count) = match days {
        ..=0 => (Day::Today, 0),
        1 => (Day::Yesterday, 1),
        days if days < week => (Day::DaysAgo, days),
        days => (Day::WeeksAgo, days / week),
    };
    DayAgo { day, count: count_of(count) }
}

pub const fn ending(outcome: Outcome) -> Ending {
    match outcome {
        Outcome::Answered => Ending::Answered,
        Outcome::Lost => Ending::Lost,
        Outcome::Missed => Ending::Missed,
        // Which of us declined is the arrow's to say, as for every other outcome.
        Outcome::Declined | Outcome::Rejected => Ending::Declined,
        Outcome::Cancelled => Ending::Cancelled,
        Outcome::NoAnswer => Ending::NoAnswer,
        Outcome::Unreachable => Ending::Unreachable,
        Outcome::Incompatible => Ending::NeedsUpdate,
        Outcome::Failed => Ending::Failed,
    }
}

/// The Calls screen, newest first, grouped by day.
pub fn call_items(
    records: &[Logged],
    contacts: &Contacts,
    clock: &LocalClock,
    selected: &FxHashSet<CallId>,
) -> Vec<CallItem> {
    let mut previous: Option<i64> = None;
    records
        .iter()
        .map(|logged| {
            let record = &logged.call;
            let days = clock.days_ago(record.at);
            let day = (previous != Some(days)).then(|| day_ago(days));
            previous = Some(days);
            let name = name_of(contacts, &record.peer);
            CallItem {
                initial: initial(&name),
                name: name.into(),
                id: record.peer.to_string().into(),
                entry: logged.id.to_string().into(),
                tint: 0,
                ending: ending(record.outcome),
                incoming: record.incoming,
                voice: record.mode == Mode::Voice,
                duration: maybe(answered_for(record.outcome, record.duration)),
                time: clock.clock_of(record.at).into(),
                selected: selected.contains(&logged.id),
                day: maybe(day),
            }
        })
        .collect()
}

/// How long an answered call lasted, when that was kept.
fn answered_for(outcome: Outcome, duration: Option<Duration>) -> Option<SharedString> {
    duration.filter(|_| outcome.answered()).map(|duration| minutes_seconds(duration).into())
}

fn minutes_seconds(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / SECONDS_PER_MINUTE, seconds % SECONDS_PER_MINUTE)
}

/// Everything the call details page shows. `saved` is the contact's name, if they are one.
pub fn call_detail(logged: &Logged, saved: Option<String>, clock: &LocalClock) -> CallDetail {
    let record = &logged.call;
    let known = saved.is_some();
    let name = saved.unwrap_or_else(|| short(&record.peer));
    let quality = record.quality.as_ref();
    CallDetail {
        entry: logged.id.to_string().into(),
        peer: record.peer.to_string().into(),
        initial: initial(&name),
        name: name.into(),
        known,
        fingerprint: fingerprint_lines(&record.peer),
        ending: ending(record.outcome),
        incoming: record.incoming,
        voice_call: record.mode == Mode::Voice,
        day: day_ago(clock.days_ago(record.at)),
        time: clock.clock_of(record.at).into(),
        duration: maybe(record.duration.map(|duration| minutes_seconds(duration).into())),
        video_from: maybe(record.video_from.map(|at| minutes_seconds(at).into())),
        rejoins: quality.map_or(0, |q| i32::try_from(q.rejoins).unwrap_or(i32::MAX)),
        traffic: maybe(record.traffic.map(|traffic| Traffic {
            sent: data_size(traffic.sent).into(),
            received: data_size(traffic.received).into(),
        })),
        video: list(quality.map(video_stats).unwrap_or_default()),
        voice: list(quality.map(voice_stats).unwrap_or_default()),
        path: maybe(quality.and_then(path)),
        ipv6: maybe(quality.filter(|q| q.paths_recorded).map(ipv6)),
        network: list(quality.map(network_stats).unwrap_or_default()),
    }
}

/// Bytes the way a phone's data usage says them: decimal units, one decimal place past a KB.
fn data_size(bytes: u64) -> String {
    const UNITS: [(u64, &str); 3] = [(1_000_000_000, "GB"), (1_000_000, "MB"), (1_000, "KB")];
    const TENTHS: u64 = 10;
    for (unit, name) in UNITS {
        if bytes >= unit {
            // Whole tenths by integer division, so no float rounding decides the last digit.
            let tenths = bytes.saturating_mul(TENTHS) / unit;
            return format!("{}.{} {name}", tenths / TENTHS, tenths % TENTHS);
        }
    }
    format!("{bytes} B")
}

fn stat(measure: Measure, figures: impl IntoIterator<Item = String>) -> Stat {
    Stat { measure, figures: list(figures.into_iter().map(SharedString::from).collect()) }
}

/// Average, lowest and highest over a call's samples; nothing for a call too short to sample.
fn spread(measure: Measure, spread: &Spread, scale: f64, decimals: usize) -> Option<Stat> {
    let mean = spread.mean()?;
    let figure = |value: f64| format!("{:.decimals$}", value / scale);
    Some(stat(measure, [figure(mean), figure(spread.min), figure(spread.max)]))
}

fn video_stats(q: &Quality) -> Vec<Stat> {
    let target = q.target.map(|t| {
        let mbps = format!("{:.1}", f64::from(t.kbps) / KBPS_PER_MBPS);
        stat(Measure::Target, [t.height.to_string(), t.fps.to_string(), mbps])
    });
    let video = &q.video;
    target
        .into_iter()
        .chain(spread(Measure::FpsOut, &q.fps_out, 1.0, WHOLE))
        .chain(spread(Measure::FpsIn, &q.fps_in, 1.0, WHOLE))
        .chain([
            stat(Measure::Frames, [video.sent.to_string(), video.received.to_string()]),
            stat(
                Measure::FramesLost,
                [video.late.to_string(), video.congested.to_string(), video.discarded.to_string()],
            ),
        ])
        .collect()
}

fn voice_stats(q: &Quality) -> Vec<Stat> {
    let audio = &q.audio;
    vec![
        stat(Measure::AudioPackets, [audio.sent.to_string(), audio.received.to_string()]),
        stat(Measure::AudioRepaired, [audio.rebuilt.to_string(), audio.concealed.to_string(), audio.late.to_string()]),
    ]
}

fn network_stats(q: &Quality) -> Vec<Stat> {
    [
        spread(Measure::SpeedUp, &q.kbps_up, KBPS_PER_MBPS, ONE_PLACE),
        spread(Measure::SpeedDown, &q.kbps_down, KBPS_PER_MBPS, ONE_PLACE),
        spread(Measure::RoundTrip, &q.rtt_ms, 1.0, WHOLE),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn percent(share: f64) -> i32 {
    count_of((share * PERCENT).round() as i64)
}

/// Which way the media went. A call from before paths were recorded by family knows only
/// whether it was relayed.
fn path(q: &Quality) -> Option<Path> {
    let path = |kind| Path { kind, v6: 0, v4: 0, relay: 0 };
    if !q.paths_recorded {
        return match q.relayed_share()? {
            share if share <= 0.0 => Some(path(PathKind::Direct)),
            share if share >= 1.0 => Some(path(PathKind::Relayed)),
            share => Some(Path { relay: percent(share), ..path(PathKind::PartlyRelayed) }),
        };
    }
    let ((v4, v6), relay) = (q.direct_shares()?, q.relayed_share()?);
    let kind = match (v6 > 0.0, v4 > 0.0, relay > 0.0) {
        (false, false, false) => return None,
        (false, false, true) => PathKind::Relayed,
        (true, false, false) => PathKind::DirectV6,
        (false, true, false) => PathKind::DirectV4,
        _ => PathKind::Mixed,
    };
    Some(Path { kind, v6: percent(v6), v4: percent(v4), relay: percent(relay) })
}

/// What happened with IPv6: used, or where it stopped — the question a relayed call raises.
fn ipv6(q: &Quality) -> Ipv6 {
    let used = q.direct_shares().is_some_and(|(_, v6)| v6 > 0.0);
    match (q.we_offered_v6, q.they_offered_v6, q.v6_path_opened) {
        _ if used => Ipv6::Used,
        (_, _, true) => Ipv6::Opened,
        (true, true, false) => Ipv6::NeverOpened,
        (true, false, _) => Ipv6::TheyHadNone,
        (false, true, _) => Ipv6::YouHadNone,
        (false, false, _) => Ipv6::Neither,
    }
}

pub const fn reach(reach: CoreReach) -> Reach {
    match reach {
        CoreReach::Online => Reach::Online,
        CoreReach::Connecting => Reach::Connecting,
        CoreReach::NoNetwork => Reach::NoNetwork,
        CoreReach::Offline => Reach::Offline,
    }
}

/// Whose side a stalled call is on, as far as this phone can tell: no network is a fact; our own
/// network just moved, or our relay being gone, means it could be either of us; otherwise it is
/// most likely them.
pub const fn stall(reach: CoreReach, moved: bool) -> Stall {
    match reach {
        CoreReach::NoNetwork => Stall::Offline,
        CoreReach::Online if !moved => Stall::Waiting,
        CoreReach::Online | CoreReach::Connecting | CoreReach::Offline => Stall::Reconnecting,
    }
}

pub const fn weak(weak: CoreWeak) -> Weak {
    match weak {
        CoreWeak::None => Weak::None,
        CoreWeak::Ours => Weak::Ours,
        CoreWeak::Theirs => Weak::Theirs,
    }
}

pub const fn route(route: MediaRoute) -> Route {
    match route {
        MediaRoute::Unknown => Route::Unknown,
        MediaRoute::Direct => Route::Direct,
        MediaRoute::Relay => Route::Relayed,
    }
}

/// A quality step as the Call quality page and its picker say it.
pub fn preset_item(preset: CorePreset) -> PresetItem {
    let video = preset.video();
    let int = |value: u32| i32::try_from(value).unwrap_or(i32::MAX);
    PresetItem {
        preset: match preset {
            CorePreset::Lowest => Preset::Lowest,
            CorePreset::Low => Preset::Low,
            CorePreset::Balanced => Preset::Balanced,
            CorePreset::High => Preset::High,
            CorePreset::Highest => Preset::Highest,
        },
        lines: int(video.height),
        fps: int(video.fps),
        megabytes: int(preset.megabytes_a_minute()),
    }
}

/// The markup's step back as the core's.
pub const fn core_preset(preset: Preset) -> CorePreset {
    match preset {
        Preset::Lowest => CorePreset::Lowest,
        Preset::Low => CorePreset::Low,
        Preset::Balanced => CorePreset::Balanced,
        Preset::High => CorePreset::High,
        Preset::Highest => CorePreset::Highest,
    }
}

/// Where the call's sound can go, for the Audio key and its sheet, in Telecom's order.
pub fn outputs(routes: &[AndroidRoute]) -> ModelRc<OutputItem> {
    let items: Vec<OutputItem> = routes
        .iter()
        .map(|route| OutputItem {
            output: match route.kind {
                RouteKind::Phone => Output::Phone,
                RouteKind::Speaker => Output::Speaker,
                RouteKind::Bluetooth => Output::Bluetooth,
                RouteKind::Wired => Output::Wired,
            },
            name: route.name.as_str().into(),
        })
        .collect();
    ModelRc::new(VecModel::from(items))
}

/// One relay on the relay page. What it is doing comes from the endpoint's report, and before
/// the first one it is doing nothing we know of.
pub fn relay_item(relay: &relays::Relay, choice: &Choice, ranking: &Ranking, live: Option<&RelayView>) -> RelayItem {
    let region = match relay.region {
        RelayRegion::India => Region::India,
        RelayRegion::NorthAmericaEast => Region::NorthAmericaEast,
        RelayRegion::NorthAmericaWest => Region::NorthAmericaWest,
        RelayRegion::Europe => Region::Europe,
        RelayRegion::AsiaPacific => Region::AsiaPacific,
        RelayRegion::Elsewhere => Region::Elsewhere,
        RelayRegion::Yours => Region::Yours,
    };
    let used = match live {
        Some(view) if view.home.as_ref() == Some(&relay.url) => RelayUse::InUse,
        Some(view) if view.active.contains(&relay.url) => RelayUse::Standby,
        _ => RelayUse::None,
    };
    RelayItem {
        url: relay.url.to_string().into(),
        host: relay.host().into(),
        name: relay.name.as_str().into(),
        region,
        ticked: choice.ticked.contains(&relay.url),
        rtt: maybe(ranking.rtt(&relay.url).map(|rtt| count_of(rtt.as_millis()))),
        r#use: used,
    }
}

/// When the last relay survey ran; empty if none has.
pub fn checked(ranking: &Ranking) -> ModelRc<Ago> {
    maybe(ranking.at.map(ago))
}

pub const fn permission(permission: AndroidPermission) -> Permission {
    match permission {
        AndroidPermission::Camera => Permission::Camera,
        AndroidPermission::RecordAudio => Permission::Microphone,
        AndroidPermission::PostNotifications => Permission::Notifications,
    }
}

/// Says something went wrong and gets out of the way; the markup's own Timer dismisses it.
/// `subject` is whatever the sentence names: a contact, or an error's own text.
pub fn toast(ui: &App, say: Say, subject: impl ToString) {
    say_counted(ui, Toast { say, subject: subject.to_string().into(), count: 0, total: 0 });
}

pub fn say_counted(ui: &App, toast: Toast) {
    tracing::info!(say = ?toast.say, subject = %toast.subject, count = toast.count, total = toast.total, "toast");
    ui.set_toast(one(toast));
}
