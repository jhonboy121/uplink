//! uplink CLI: identity, contacts and calls through the same core the app uses.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::Dispatch;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use uplink_core::contacts::Contacts;
use uplink_core::node::{Command, Event, Network, Node};
use uplink_core::{EndpointId, identity, runtime};

const USAGE: &str = "\
usage: uplink [--dir <path>] <command>
  id                  print this device's key
  contacts            list contacts
  add <name> <key>    add a contact
  remove <name>       remove a contact
  listen              wait for calls   (a accept, r reject, h hang up, q quit)
  call <name|key>     call someone     (h hang up, q quit)
data dir: --dir, else $UPLINK_DIR, else ~/.local/share/uplink; log filter: $UPLINK_LOG";
const DIR_FLAG: &str = "--dir";
const DIR_ENV: &str = "UPLINK_DIR";
const LOG_ENV: &str = "UPLINK_LOG";
const DEFAULT_LOG: &str = "warn,uplink=info,uplink_core=info";
const DEFAULT_DIR: &str = ".local/share/uplink";

enum Cli {
    Id,
    Contacts,
    Add { name: String, key: String },
    Remove { name: String },
    Listen,
    Call { target: String },
}

fn parse(args: &[String]) -> Result<(PathBuf, Cli)> {
    let (dir, rest) = match args {
        [flag, dir, rest @ ..] if flag == DIR_FLAG => (PathBuf::from(dir), rest),
        rest => (default_dir()?, rest),
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let cli = match rest.as_slice() {
        ["id"] => Cli::Id,
        ["contacts"] => Cli::Contacts,
        ["add", name, key] => Cli::Add { name: (*name).to_owned(), key: (*key).to_owned() },
        ["remove", name] => Cli::Remove { name: (*name).to_owned() },
        ["listen"] => Cli::Listen,
        ["call", target] => Cli::Call { target: (*target).to_owned() },
        _ => return Err(anyhow!("{USAGE}")),
    };
    Ok((dir, cli))
}

fn default_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("no --dir, $UPLINK_DIR or $HOME")?;
    Ok(PathBuf::from(home).join(DEFAULT_DIR))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dir, cli) = parse(&args)?;
    let filter = std::env::var(LOG_ENV).unwrap_or_else(|_| DEFAULT_LOG.to_owned());
    let dispatch = Dispatch::new(
        tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(Targets::from_str(&filter)?),
    );
    let _log = tracing::dispatcher::set_default(&dispatch);
    let runtime = runtime::build(dispatch)?;
    let outcome = runtime.block_on(run(&dir, cli));
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
            contacts.add(&name, EndpointId::from_str(&key)?)?;
            contacts.save().await?;
        }
        Cli::Remove { name } => {
            let mut contacts = Contacts::load(dir).await?;
            contacts.remove(&name)?;
            contacts.save().await?;
        }
        Cli::Listen => session(dir, None).await?,
        Cli::Call { target } => {
            let peer = Contacts::load(dir).await?.resolve(&target)?;
            session(dir, Some(peer)).await?;
        }
    }
    Ok(())
}

/// Runs the node until quit; when placing a call, also until that call ends.
async fn session(dir: &Path, call: Option<EndpointId>) -> Result<()> {
    let contacts = Contacts::load(dir).await?;
    let (node, mut events) = Node::start(identity::load_or_create(dir).await?, Network::N0).await?;
    if let Some(peer) = call {
        node.send(Command::Call(peer)).await?;
    }
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let outcome = loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    println!("{}", describe(&event, &contacts));
                    if call.is_some() && matches!(event, Event::Ended { .. }) {
                        break Ok(());
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
    node.shutdown().await;
    outcome
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
