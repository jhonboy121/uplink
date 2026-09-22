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
use uplink_core::node::{Command, Event, Network, Node};
use uplink_core::{EndpointId, identity, runtime};

// mp4_atom warns about every vendor box in a phone recording (`smta`, `cami`, …); not our problem.
const DEFAULT_LOG: &str = "warn,uplink=info,uplink_core=info,mp4_atom=error";
const DEFAULT_DIR: &str = ".local/share/uplink";

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
    /// Wait for calls (a accept, r reject, h hang up, q quit)
    Listen {
        #[command(flatten)]
        media: MediaArgs,
    },
    /// Call a contact name or key (h hang up, q quit)
    Call {
        target: String,
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
            for contact in Contacts::load(dir).await?.iter() {
                println!("{}\t{}", contact.name, contact.id);
            }
        }
        Cli::Add { name, key } => {
            let mut contacts = Contacts::load(dir).await?;
            contacts.add(&name, key)?;
            contacts.save().await?;
        }
        Cli::Remove { name } => {
            let mut contacts = Contacts::load(dir).await?;
            contacts.remove(&name)?;
            contacts.save().await?;
        }
        Cli::Listen { media } => session(dir, None, media).await?,
        Cli::Call { target, media } => {
            let peer = Contacts::load(dir).await?.resolve(&target)?;
            session(dir, Some(peer), media).await?;
        }
    }
    Ok(())
}

/// Runs the node until quit; when placing a call, also until that call ends.
async fn session(dir: &Path, call: Option<EndpointId>, media_args: MediaArgs) -> Result<()> {
    let MediaArgs { video, record } = media_args;
    if let Some(path) = &video {
        // Fail early on a bad clip; each call reopens it to start from the beginning.
        clip::Clip::open(path)?;
    }
    let contacts = Contacts::load(dir).await?;
    let (node, mut events) = Node::start(identity::load_or_create(dir).await?, Network::N0).await?;
    if let Some(peer) = call {
        node.send(Command::Call(peer)).await?;
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
        "q" => return Ok(false),
        "" => return Ok(true),
        other => {
            println!("unknown input `{other}` (a accept, r reject, h hang up, q quit)");
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
        Event::Online => "online".to_owned(),
        Event::Dialing { peer } => format!("dialing {}", name(contacts, peer)),
        Event::Ringing { peer } => format!("ringing {}", name(contacts, peer)),
        Event::Incoming { peer } => format!("incoming call from {} (a accept, r reject)", name(contacts, peer)),
        Event::Connected { peer, key_exchange, .. } => format!("connected to {} [{key_exchange:?}]", name(contacts, peer)),
        Event::Ended { peer, reason } => {
            let who = peer.as_ref().map_or_else(|| "unknown peer".to_owned(), |p| name(contacts, p));
            format!("call with {who} ended: {reason:?}")
        }
    }
}
