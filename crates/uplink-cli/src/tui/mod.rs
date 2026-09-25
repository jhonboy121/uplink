//! A phone in the terminal: calls with the controls the app has (mic, camera, hold, voice to
//! video), its call quality steps per network, and its rate control driving a live encoder, so
//! the far end sees what a phone would send.

pub mod log;
mod view;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::crossterm::event::{self, Event as Term, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use uplink_core::contacts::Contacts;
use uplink_core::db::Db;
use uplink_core::media::MediaStats;
use uplink_core::node::{Command, Event, MediaState, Mode, Network, Node, Steer};
use uplink_core::preset::{Network as Metered, Preset};
use uplink_core::rate::{Change, Rate, Reading};
use uplink_core::reach::Reach;
use uplink_core::settings::Settings;
use uplink_core::{EndpointId, identity};

use crate::clip::Clip;
use crate::live::{self, Live, Plan};
use crate::record::Recorder;
use crate::{APP, describe};

/// How often the screen is drawn without anything happening: the stats move every second.
const REDRAW: Duration = Duration::from_millis(250);
/// Rate control's sample, as the app takes it.
const PACE: Duration = Duration::from_secs(1);
/// How long quitting waits for a call to hang up.
const HANG_UP_WAIT: Duration = Duration::from_secs(2);
/// Lines the log pane keeps.
const LOG_LINES: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Dialing,
    Ringing,
    Incoming,
    Connected,
    Reconnecting,
}

struct Call {
    peer: EndpointId,
    mode: Mode,
    phase: Phase,
    key_exchange: Option<String>,
    connected_at: Option<Instant>,
    theirs: MediaState,
    /// They asked to switch to video and we haven't answered.
    asked_us: bool,
    /// We asked them.
    we_asked: bool,
    live: Option<Live>,
    stats: Option<Arc<MediaStats>>,
    rate: Option<Rate>,
}

impl Call {
    const fn new(peer: EndpointId, mode: Mode, phase: Phase) -> Self {
        Self {
            peer,
            mode,
            phase,
            key_exchange: None,
            connected_at: None,
            theirs: MediaState { mic_off: false, camera_off: false, held: false },
            asked_us: false,
            we_asked: false,
            live: None,
            stats: None,
            rate: None,
        }
    }
}

struct Tui {
    node: Node,
    contacts: Contacts,
    selected: usize,
    me: Option<EndpointId>,
    reach: Option<Reach>,
    relays: String,
    settings: Settings,
    /// The network this "phone" says it is on, for the quality step: Wi-Fi or mobile data.
    network: Metered,
    call: Option<Call>,
    mic_on: bool,
    camera_on: bool,
    held: bool,
    clip: Option<PathBuf>,
    record: Option<PathBuf>,
    log: VecDeque<String>,
    quit: bool,
}

/// Runs until quit. `logs` is the log sink's pane end.
pub async fn run(
    dir: &Path,
    clip: Option<PathBuf>,
    record: Option<PathBuf>,
    logs: mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    if let Some(path) = &clip {
        // Fail before the screen takes over, where the error can be read.
        live::check_clip(path)?;
    }
    let db = Db::open(dir)?;
    let contacts = Contacts::open(db.clone())?;
    let settings = Settings::open(db)?;
    let (node, events) =
        Node::start(identity::load_or_create(dir).await?, Network::Public(settings.clone()), APP).await?;
    let tui = Tui {
        node,
        contacts,
        selected: 0,
        me: None,
        reach: None,
        relays: "no survey yet".to_owned(),
        settings,
        network: Metered::Wifi,
        call: None,
        mic_on: true,
        camera_on: true,
        held: false,
        clip,
        record,
        log: VecDeque::new(),
        quit: false,
    };
    let mut terminal = ratatui::init();
    let outcome = tui.drive(&mut terminal, events, logs).await;
    ratatui::restore();
    outcome
}

impl Tui {
    async fn drive(
        mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut events: mpsc::Receiver<Event>,
        mut logs: mpsc::UnboundedReceiver<String>,
    ) -> Result<()> {
        let (keys_tx, mut keys) = mpsc::unbounded_channel();
        // crossterm's read blocks; the thread goes with the process.
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if keys_tx.send(event).is_err() {
                    break;
                }
            }
        });
        let mut redraw = tokio::time::interval(REDRAW);
        let mut pace = tokio::time::interval(PACE);
        let outcome = loop {
            terminal.draw(|frame| view::draw(frame, &self))?;
            tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.on_event(event),
                    None => break Ok(()),
                },
                Some(key) = keys.recv() => {
                    if let Term::Key(key) = key
                        && let Err(e) = self.on_key(key).await
                    {
                        break Err(e);
                    }
                }
                Some(line) = logs.recv() => self.say(line),
                _ = pace.tick() => self.pace(),
                _ = redraw.tick() => {}
            }
            if self.quit {
                break Ok(());
            }
        };
        if self.call.is_some() {
            self.hang_up(&mut events).await;
        }
        self.call = None;
        self.node.shutdown().await;
        outcome
    }

    /// Quitting mid-call: hang up and wait a moment for it to go, so they see a hang-up and not
    /// a lost connection.
    async fn hang_up(&mut self, events: &mut mpsc::Receiver<Event>) {
        if self.send(Command::Hangup).await.is_err() {
            return;
        }
        let ended = async {
            while let Some(event) = events.recv().await {
                if matches!(event, Event::Ended { .. }) {
                    break;
                }
            }
        };
        if tokio::time::timeout(HANG_UP_WAIT, ended).await.is_err() {
            tracing::warn!("quit before the hang-up went");
        }
    }

    fn say(&mut self, line: String) {
        if self.log.len() == LOG_LINES {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn peers(&self) -> Vec<(String, EndpointId)> {
        self.contacts.iter().map(|contact| (contact.name.clone(), contact.id)).collect()
    }

    fn name(&self, id: &EndpointId) -> String {
        crate::name(&self.contacts, id)
    }

    /// The cap this network's chosen step puts on the call. The CLI can send every step.
    fn cap(&self) -> Preset {
        Preset::chosen(&self.settings, self.network)
    }

    fn media_state(&self) -> MediaState {
        let video = self.call.as_ref().is_some_and(|call| call.mode == Mode::Video);
        MediaState { mic_off: !self.mic_on, camera_off: !(self.camera_on && video), held: self.held }
    }

    fn plan(&self) -> Option<Plan> {
        let call = self.call.as_ref()?;
        let rate = call.rate.as_ref()?;
        Some(Plan {
            step: rate.step(),
            kbps: rate.kbps(),
            video: call.mode == Mode::Video && self.camera_on && !self.held,
            voice: self.mic_on && !self.held,
            playout: !self.held,
            voice_bps: self.cap().voice_bps(),
        })
    }

    /// Tells the media what the controls say now.
    fn replan(&self) {
        if let (Some(plan), Some(live)) = (self.plan(), self.call.as_ref().and_then(|call| call.live.as_ref())) {
            live.plan.send_if_modified(|now| std::mem::replace(now, plan) != plan);
        }
    }

    fn on_event(&mut self, event: Event) {
        tracing::info!("{}", describe(&event, &self.contacts));
        match event {
            Event::Ready { id } => self.me = Some(id),
            Event::Reach(reach) => self.reach = Some(reach),
            Event::Relays(view) => {
                self.relays = view.home.as_ref().map_or_else(|| "no home relay".to_owned(), ToString::to_string);
            }
            Event::Dialing { peer, mode } => self.call = Some(Call::new(peer, mode, Phase::Dialing)),
            Event::Ringing { .. } => self.set_phase(Phase::Ringing),
            Event::Incoming { peer, mode } => {
                if self.call.is_none() {
                    self.call = Some(Call::new(peer, mode, Phase::Incoming));
                }
            }
            Event::Connected { peer, key_exchange, mode, media } => {
                self.connected(peer, format!("{key_exchange:?}"), mode, *media)
            }
            Event::PeerMedia(theirs) => {
                if let Some(call) = &mut self.call {
                    call.theirs = theirs;
                }
            }
            Event::VideoAsked(asked) => {
                if let Some(call) = &mut self.call {
                    call.asked_us = asked;
                }
            }
            Event::VideoOn => {
                if let Some(call) = &mut self.call {
                    (call.mode, call.asked_us, call.we_asked) = (Mode::Video, false, false);
                }
                self.replan();
                self.tell_media();
            }
            Event::VideoDeclined => {
                if let Some(call) = &mut self.call {
                    call.we_asked = false;
                }
            }
            Event::Reconnecting => self.set_phase(Phase::Reconnecting),
            Event::Reconnected => self.set_phase(Phase::Connected),
            Event::Ended { .. } => {
                self.call = None;
                (self.mic_on, self.camera_on, self.held) = (true, true, false);
            }
            Event::Network(_) => {}
        }
    }

    const fn set_phase(&mut self, phase: Phase) {
        if let Some(call) = &mut self.call {
            call.phase = phase;
        }
    }

    fn connected(
        &mut self,
        peer: EndpointId,
        key_exchange: String,
        mode: Mode,
        media: uplink_core::media::MediaSession,
    ) {
        let clip = self.clip.as_deref().map(Clip::open).transpose().unwrap_or_else(|e| {
            tracing::warn!("clip unavailable, sending the test pattern: {e:#}");
            None
        });
        let recorder = self.record.as_deref().map(Recorder::create).transpose().unwrap_or_else(|e| {
            tracing::warn!("not recording: {e:#}");
            None
        });
        let stats = Arc::clone(&media.stats);
        let rate = Rate::new(self.cap(), &Preset::ALL);
        stats.video_kbps.store(u64::from(rate.kbps()), Ordering::Relaxed);
        let call = self.call.get_or_insert_with(|| Call::new(peer, mode, Phase::Connected));
        (call.peer, call.mode, call.phase) = (peer, mode, Phase::Connected);
        call.key_exchange = Some(key_exchange);
        call.connected_at = Some(Instant::now());
        call.rate = Some(rate);
        call.stats = Some(stats);
        if let Some(plan) = self.plan() {
            let live = live::start(media, clip, recorder, plan);
            if let Some(call) = &mut self.call {
                call.live = Some(live);
            }
        }
        self.tell_media();
    }

    /// Rate control's second, as the app's: paused while the call has no path to judge.
    fn pace(&mut self) {
        let Some(call) = &mut self.call else { return };
        let (Some(rate), Some(stats)) = (&mut call.rate, &call.stats) else { return };
        if stats.stalled() || call.phase == Phase::Reconnecting {
            rate.pause();
            return;
        }
        match rate.sample(Reading::read(stats, Instant::now())) {
            Change::None => return,
            Change::Bitrate(_) => {}
            Change::Step(step, kbps) => {
                tracing::info!(?step, kbps, "call picture steps");
                stats.step_changes.fetch_add(1, Ordering::Relaxed);
            }
        }
        stats.video_kbps.store(u64::from(rate.kbps()), Ordering::Relaxed);
        self.replan();
    }

    /// A new network or a new choice: rate control starts over at the new cap, as on the phone.
    fn requalify(&mut self) {
        let cap = self.cap();
        tracing::info!(network = ?self.network, ?cap, "call quality");
        if let Some(call) = &mut self.call
            && let Some(stats) = &call.stats
        {
            let rate = Rate::new(cap, &Preset::ALL);
            stats.video_kbps.store(u64::from(rate.kbps()), Ordering::Relaxed);
            call.rate = Some(rate);
        }
        self.replan();
    }

    fn tell_media(&self) {
        if self
            .call
            .as_ref()
            .is_some_and(|call| call.phase == Phase::Connected || call.phase == Phase::Reconnecting)
            && let Err(e) = self.node.handle().try_send(Command::Media(self.media_state()))
        {
            tracing::warn!("telling them our media: {e:#}");
        }
    }

    async fn send(&self, command: Command) -> Result<()> {
        Ok(self.node.send(command).await?)
    }

    async fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return Ok(());
        }
        let phase = self.call.as_ref().map(|call| call.phase);
        let asked_us = self.call.as_ref().is_some_and(|call| call.asked_us);
        match (key.code, phase) {
            (KeyCode::Char('q'), _) => self.quit = true,
            (KeyCode::Up | KeyCode::Char('k'), None) => self.selected = self.selected.saturating_sub(1),
            (KeyCode::Down | KeyCode::Char('j'), None) => {
                self.selected = (self.selected + 1).min(self.peers().len().saturating_sub(1));
            }
            (KeyCode::Enter | KeyCode::Char('v'), None) => self.dial(Mode::Video).await?,
            (KeyCode::Char('a'), None) => self.dial(Mode::Voice).await?,
            (KeyCode::Enter | KeyCode::Char('y'), Some(Phase::Incoming)) => self.send(Command::Answer(true)).await?,
            (KeyCode::Char('n'), Some(Phase::Incoming)) => self.send(Command::Answer(false)).await?,
            (KeyCode::Char('y'), Some(_)) if asked_us => self.answer_video(true).await?,
            (KeyCode::Char('n'), Some(_)) if asked_us => self.answer_video(false).await?,
            (KeyCode::Char('h'), Some(_)) => self.send(Command::Hangup).await?,
            (KeyCode::Char('m'), Some(_)) => {
                self.mic_on = !self.mic_on;
                self.controls_changed();
            }
            (KeyCode::Char('c'), Some(_)) => {
                self.camera_on = !self.camera_on;
                self.controls_changed();
            }
            (KeyCode::Char('p'), Some(_)) => {
                self.held = !self.held;
                tracing::info!(held = self.held, "call hold");
                self.controls_changed();
            }
            (KeyCode::Char('u'), Some(Phase::Connected)) => {
                if let Some(call) = &mut self.call
                    && call.mode == Mode::Voice
                {
                    call.we_asked = !call.we_asked;
                    let ask = call.we_asked;
                    self.send(Command::AskVideo(ask)).await?;
                }
            }
            (KeyCode::Char('w'), _) => {
                self.network = match self.network {
                    Metered::Wifi => Metered::Mobile,
                    Metered::Mobile => Metered::Wifi,
                };
                // What the app says when Android hands it another network.
                self.send(Command::Network(true)).await?;
                self.requalify();
            }
            (KeyCode::Char('[' | '-'), _) => self.choose(-1)?,
            (KeyCode::Char(']' | '+' | '='), _) => self.choose(1)?,
            (KeyCode::Char('r'), _) => self.send(Command::Relays(Steer::Check)).await?,
            _ => {}
        }
        Ok(())
    }

    fn controls_changed(&self) {
        self.replan();
        self.tell_media();
    }

    async fn dial(&mut self, mode: Mode) -> Result<()> {
        let peers = self.peers();
        let Some((_, peer)) = peers.get(self.selected) else {
            self.say("no contacts: add one with `uplink add <name> <key>`".to_owned());
            return Ok(());
        };
        self.send(Command::Call(*peer, mode)).await
    }

    async fn answer_video(&mut self, yes: bool) -> Result<()> {
        if let Some(call) = &mut self.call {
            call.asked_us = false;
        }
        self.send(Command::AnswerVideo(yes)).await
    }

    /// Steps this network's chosen quality up or down, saved as the app saves it.
    fn choose(&mut self, by: isize) -> Result<()> {
        let now = Preset::ALL.iter().position(|preset| *preset == self.cap()).unwrap_or_default();
        let Some(next) = now.checked_add_signed(by).and_then(|index| Preset::ALL.get(index)) else { return Ok(()) };
        next.choose(&self.settings, self.network)?;
        self.requalify();
        Ok(())
    }
}
