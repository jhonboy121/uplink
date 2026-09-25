//! uplink CLI: identity and contacts, and a TUI that places and takes calls as a phone would,
//! through the same core the app uses.

mod clip;
mod live;
mod record;
mod tui;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::Dispatch;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use uplink_core::contacts::Contacts;
use uplink_core::db::Db;
use uplink_core::node::{Behind, EndReason, Event};
use uplink_core::{EndpointId, identity, runtime};

// mp4_atom warns about every vendor box in a phone recording (`smta`, `cami`, …); not our problem.
/// iroh's path events and the selector's RTTs too: why a call ran over the path it did.
const DEFAULT_LOG: &str = "warn,uplink=info,uplink_core=info,mp4_atom=error,\
    iroh::_events::path=debug,iroh::socket::biased_rtt_path_selector=trace";
const DEFAULT_DIR: &str = ".local/share/uplink";
/// What this build tells the other side of a call it is, for either side's "update" notice.
const APP: &str = concat!("cli ", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(name = "uplink", about = "uplink peer-to-peer calls from the terminal; without a command, the call TUI")]
struct Args {
    /// Data dir (identity, contacts, settings, uplink.log) [default: ~/.local/share/uplink]
    #[arg(long, env = "UPLINK_DIR", global = true)]
    dir: Option<PathBuf>,
    /// tracing filter for the log
    #[arg(long, env = "UPLINK_LOG", default_value = DEFAULT_LOG, global = true)]
    log: String,
    /// H.264 + AAC mp4 played as our camera and microphone, looped [default: a test pattern]
    #[arg(long)]
    video: Option<PathBuf>,
    /// Record the peer's video and audio to this mp4
    #[arg(long)]
    record: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cli>,
}

#[derive(Subcommand)]
enum Cli {
    /// Print this device's key
    Id,
    /// List contacts
    Contacts,
    /// Add a contact
    Add { name: String, key: EndpointId },
    /// Remove a contact
    Remove { name: String },
}

fn default_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no --dir, $UPLINK_DIR or $HOME")?;
    Ok(PathBuf::from(home).join(DEFAULT_DIR))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dir = args.dir.map_or_else(default_dir, Ok)?;
    let filter = Targets::from_str(&args.log)?;
    let Some(command) = args.command else {
        // The screen is the TUI's: logs go to a file in the data dir and the log pane.
        let (sink, pane) = tui::log::Sink::open(&dir)?;
        let dispatch = Dispatch::new(
            tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(sink)).with(filter),
        );
        let _log = tracing::dispatcher::set_default(&dispatch);
        let runtime = runtime::build(dispatch)?;
        let outcome = runtime.block_on(tui::run(&dir, args.video, args.record, pane));
        runtime.shutdown_background();
        return outcome;
    };
    let dispatch =
        Dispatch::new(tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr)).with(filter));
    let _log = tracing::dispatcher::set_default(&dispatch);
    let runtime = runtime::build(dispatch)?;
    runtime.block_on(run(&dir, command))
}

async fn run(dir: &Path, cli: Cli) -> Result<()> {
    match cli {
        Cli::Id => println!("{}", identity::load_or_create(dir).await?.public()),
        Cli::Contacts => {
            for contact in Contacts::open(Db::open(dir)?)?.iter() {
                let mark = if contact.favourite { "*" } else { " " };
                println!("{mark}\t{}\t{}", contact.name, contact.id);
            }
        }
        Cli::Add { name, key } => Contacts::open(Db::open(dir)?)?.add(&name, key)?,
        Cli::Remove { name } => {
            Contacts::open(Db::open(dir)?)?.remove(&name)?;
        }
    }
    Ok(())
}

fn name(contacts: &Contacts, id: &EndpointId) -> String {
    contacts.name_of(id).map_or_else(|| id.fmt_short().to_string(), str::to_owned)
}

fn describe(event: &Event, contacts: &Contacts) -> String {
    match event {
        Event::Ready { id } => format!("ready as {id}"),
        Event::Reach(reach) => format!("reach: {reach:?}"),
        Event::Network(up) => format!("network {}", if *up { "up" } else { "gone" }),
        Event::Dialing { peer, mode } => format!("dialing {} ({mode:?})", name(contacts, peer)),
        Event::Ringing { peer } => format!("ringing {}", name(contacts, peer)),
        Event::Incoming { peer, mode } => format!("incoming {mode:?} call from {}", name(contacts, peer)),
        Event::Connected { peer, key_exchange, mode, .. } => {
            format!("connected to {} [{key_exchange:?}] ({mode:?})", name(contacts, peer))
        }
        Event::PeerMedia(state) => format!("their mic off: {}, camera off: {}", state.mic_off, state.camera_off),
        Event::VideoAsked(true) => "they ask to switch to video".to_owned(),
        Event::VideoAsked(false) => "they took back the ask for video".to_owned(),
        Event::VideoOn => "switched to video".to_owned(),
        Event::VideoDeclined => "they kept it voice".to_owned(),
        Event::Reconnecting => "connection lost; reconnecting".to_owned(),
        Event::Reconnected => "reconnected".to_owned(),
        Event::Ended { peer, reason } => {
            let who = peer.as_ref().map_or_else(|| "unknown peer".to_owned(), |p| name(contacts, p));
            match reason {
                EndReason::Incompatible { behind: Behind::Us, theirs } => {
                    format!("cannot call {who}: update uplink (they run {theirs}, this is {APP})")
                }
                EndReason::Incompatible { behind: Behind::Them, theirs } => {
                    format!("cannot call {who}: they need to update uplink (they run {theirs}, this is {APP})")
                }
                reason => format!("call with {who} ended: {reason:?}"),
            }
        }
        Event::Relays(view) => {
            let ranked: Vec<String> = view
                .ranking
                .relays
                .iter()
                .map(|relay| {
                    let tag = if view.home.as_ref() == Some(&relay.url) {
                        " [in use]"
                    } else if view.active.contains(&relay.url) {
                        " [active]"
                    } else {
                        ""
                    };
                    format!("{} {} ms{tag}", relay.url, relay.rtt.as_millis())
                })
                .collect();
            let home = view.home.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string);
            format!("relays: home {home}; ranked {}", if ranked.is_empty() { "nothing yet".to_owned() } else { ranked.join(", ") })
        }
    }
}
