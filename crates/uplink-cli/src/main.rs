//! uplink CLI: identity, contacts and calls through the same core the app uses.

mod clip;
mod h264;
mod media;
mod record;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::Dispatch;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use uplink_core::contacts::Contacts;
use uplink_core::db::Db;
use uplink_core::node::{Behind, Command, EndReason, Event, Mode, Network, Node, Steer};
use uplink_core::settings::Settings;
use uplink_core::{EndpointId, identity, runtime};

// mp4_atom warns about every vendor box in a phone recording (`smta`, `cami`, …); not our problem.
const DEFAULT_LOG: &str = "warn,uplink=info,uplink_core=info,mp4_atom=error";
const DEFAULT_DIR: &str = ".local/share/uplink";
/// What this build tells the other side of a call it is, for either side's "update" notice.
const APP: &str = concat!("cli ", env!("CARGO_PKG_VERSION"));

#[derive(Parser)]
#[command(name = "uplink", about = "uplink peer-to-peer calls from the terminal")]
struct Args {
    /// Data dir (identity, contacts) [default: ~/.local/share/uplink]
    #[arg(long, env = "UPLINK_DIR", global = true)]
    dir: Option<PathBuf>,
    /// tracing filter for stderr logs
    #[arg(long, env = "UPLINK_LOG", default_value = DEFAULT_LOG, global = true)]
    log: String,
    #[command(subcommand)]
    command: Cli,
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
    /// Wait for calls (a accept, r reject, h hang up, v ask for video, y/n answer it, q quit)
    Listen {
        #[command(flatten)]
        media: MediaArgs,
    },
    /// Call a contact name or key (h hang up, v ask for video, y/n answer it, q quit)
    Call {
        target: String,
        /// Place it as a voice call
        #[arg(long)]
        voice: bool,
        #[command(flatten)]
        media: MediaArgs,
    },
}

#[derive(clap::Args)]
struct MediaArgs {
    /// H.264 + AAC mp4 sent as our video and voice, looped
    #[arg(long)]
    video: Option<PathBuf>,
    /// Record the peer's video and audio to this mp4
    #[arg(long)]
    record: Option<PathBuf>,
}

fn default_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("no --dir, $UPLINK_DIR or $HOME")?;
    Ok(PathBuf::from(home).join(DEFAULT_DIR))
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dir = args.dir.map_or_else(default_dir, Ok)?;
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(Targets::from_str(&args.log)?),
    );
    let _log = tracing::dispatcher::set_default(&dispatch);
    let runtime = runtime::build(dispatch)?;
    let outcome = runtime.block_on(run(&dir, args.command));
    // A pending stdin read sits on a blocking thread until Enter; don't wait for it.
    runtime.shutdown_background();
    outcome
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
        Cli::Listen { media } => session(dir, None, media).await?,
        Cli::Call { target, voice, media } => {
            let peer = Contacts::open(Db::open(dir)?)?.resolve(&target)?;
            let mode = if voice { Mode::Voice } else { Mode::Video };
            session(dir, Some((peer, mode)), media).await?;
        }
    }
    Ok(())
}

/// Runs the node until quit; when placing a call, also until that call ends.
async fn session(dir: &Path, call: Option<(EndpointId, Mode)>, media_args: MediaArgs) -> Result<()> {
    let MediaArgs { video, record } = media_args;
    if let Some(path) = &video {
        // Fail early on a bad clip; each call reopens it to start from the beginning.
        clip::Clip::open(path)?;
    }
    let db = Db::open(dir)?;
    let contacts = Contacts::open(db.clone())?;
    // The same settings the app reads, out of this data dir. The CLI is the only harness the core
    // has off a phone, so relay behaviour that cannot be tried here cannot be tried at all.
    let (node, mut events) =
        Node::start(identity::load_or_create(dir).await?, Network::Public(Settings::open(db)?), APP).await?;
    if let Some((peer, mode)) = call {
        node.send(Command::Call(peer, mode)).await?;
    }
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut call_media: Option<AbortOnDrop> = None;
    let outcome = loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    println!("{}", describe(&event, &contacts));
                    match event {
                        Event::Connected { media, .. } => {
                            let clip = video.as_deref().map(clip::Clip::open).transpose().unwrap_or_else(|e| {
                                println!("clip unavailable, sending no video: {e:#}");
                                None
                            });
                            let recorder = record.as_deref().map(record::Recorder::create).transpose()?;
                            call_media = Some(AbortOnDrop(tokio::spawn(media::run(*media, clip, recorder))));
                        }
                        Event::Ended { .. } => {
                            call_media = None;
                            if call.is_some() {
                                break Ok(());
                            }
                        }
                        _ => {}
                    }
                }
                None => break Ok(()),
            },
            line = input.next_line() => match line {
                Ok(Some(line)) => match handle_input(&node, line.trim()).await {
                    Ok(true) => {}
                    Ok(false) => break Ok(()),
                    Err(e) => break Err(e),
                },
                Ok(None) => break Ok(()),
                Err(e) => break Err(e.into()),
            },
            signal = tokio::signal::ctrl_c() => break signal.map_err(Into::into),
        }
    };
    drop(call_media);
    node.shutdown().await;
    outcome
}

/// Stops a call's media task with the call.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Returns whether to keep running.
async fn handle_input(node: &Node, line: &str) -> Result<bool> {
    let command = match line {
        "a" => Command::Answer(true),
        "r" => Command::Answer(false),
        "h" => Command::Hangup,
        "v" => Command::AskVideo(true),
        "y" => Command::AnswerVideo(true),
        "n" => Command::AnswerVideo(false),
        "c" => Command::Relays(Steer::Check),
        "q" => return Ok(false),
        "" => return Ok(true),
        other => {
            println!(
                "unknown input `{other}` (a accept, r reject, h hang up, v ask for video, y/n answer it, c check relays, q quit)"
            );
            return Ok(true);
        }
    };
    node.send(command).await?;
    Ok(true)
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
        Event::Incoming { peer, mode } => {
            format!("incoming {mode:?} call from {} (a accept, r reject)", name(contacts, peer))
        }
        Event::Connected { peer, key_exchange, mode, .. } => {
            format!("connected to {} [{key_exchange:?}] ({mode:?})", name(contacts, peer))
        }
        Event::PeerMedia(state) => format!("their mic off: {}, camera off: {}", state.mic_off, state.camera_off),
        Event::VideoAsked(true) => "they ask to switch to video (y switch, n keep voice)".to_owned(),
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
