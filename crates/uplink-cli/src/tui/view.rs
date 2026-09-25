//! Drawing the TUI: who we are, the contacts, the call and its controls, what goes each way, the log.

use std::sync::atomic::{AtomicU64, Ordering};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use uplink_core::node::Mode;
use uplink_core::preset::{Network as Metered, Preset};

use super::{Call, Phase, Tui};
use crate::live::{Encoding, Received};

const CONTACTS_WIDTH: u16 = 28;
const CALL_HEIGHT: u16 = 8;
const QUALITY_HEIGHT: u16 = 4;
const TRAFFIC_HEIGHT: u16 = 6;
const SECONDS_PER_MINUTE: u64 = 60;

pub fn draw(frame: &mut Frame, tui: &Tui) {
    let [header, body, log, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(CALL_HEIGHT),
        Constraint::Percentage(35),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [contacts, right] = Layout::horizontal([Constraint::Length(CONTACTS_WIDTH), Constraint::Min(0)]).areas(body);
    let [call, quality, traffic] = Layout::vertical([
        Constraint::Length(CALL_HEIGHT),
        Constraint::Length(QUALITY_HEIGHT),
        Constraint::Min(TRAFFIC_HEIGHT),
    ])
    .areas(right);
    frame.render_widget(Paragraph::new(header_line(tui)), header);
    draw_contacts(frame, tui, contacts);
    frame.render_widget(Paragraph::new(call_lines(tui)).block(Block::bordered().title(" call ")), call);
    frame.render_widget(Paragraph::new(quality_lines(tui)).block(Block::bordered().title(" quality ")), quality);
    frame.render_widget(
        Paragraph::new(traffic_lines(tui))
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" traffic ")),
        traffic,
    );
    draw_log(frame, tui, log);
    frame.render_widget(Paragraph::new(keys(tui)).style(Style::new().reversed()), footer);
}

fn header_line(tui: &Tui) -> Line<'static> {
    let me = tui.me.map_or_else(|| "starting…".to_owned(), |id| id.to_string());
    let reach = tui.reach.map_or_else(|| "reach unknown".to_owned(), |reach| format!("{reach:?}"));
    Line::from(vec![
        Span::styled(" uplink ", Style::new().bold().reversed()),
        Span::raw(format!(" {me} · {reach} · relay {}", tui.relays)),
    ])
}

fn draw_contacts(frame: &mut Frame, tui: &Tui, area: Rect) {
    let items: Vec<ListItem> = tui
        .peers()
        .into_iter()
        .map(|(name, id)| ListItem::new(format!("{name}  {}", id.fmt_short())))
        .collect();
    let empty = items.is_empty();
    let list = List::new(items)
        .block(Block::bordered().title(" contacts "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    let mut state = ListState::default().with_selected((!empty).then_some(tui.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn on_off(on: bool) -> Span<'static> {
    if on {
        Span::styled("on", Style::new().fg(Color::Green))
    } else {
        Span::styled("off", Style::new().fg(Color::Red))
    }
}

fn call_lines(tui: &Tui) -> Vec<Line<'static>> {
    let Some(call) = &tui.call else {
        return vec![Line::raw("no call"), Line::raw(""), Line::raw("Enter/v video call · a voice call")];
    };
    let phase = match call.phase {
        Phase::Dialing => "dialing".to_owned(),
        Phase::Ringing => "ringing".to_owned(),
        Phase::Incoming => "incoming — y accept, n reject".to_owned(),
        Phase::Connected => format!("connected {}", timer(call)),
        Phase::Reconnecting => "reconnecting…".to_owned(),
    };
    let mode = match call.mode {
        Mode::Voice => "voice",
        Mode::Video => "video",
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(tui.name(&call.peer), Style::new().bold()),
            Span::raw(format!("  {mode} · {phase}")),
            Span::raw(call.key_exchange.as_ref().map(|k| format!(" · {k}")).unwrap_or_default()),
        ]),
        Line::from(vec![
            Span::raw("ours    mic "),
            on_off(tui.mic_on),
            Span::raw("  camera "),
            on_off(tui.camera_on && call.mode == Mode::Video),
            Span::raw("  hold "),
            on_off(tui.held),
        ]),
        Line::from(vec![
            Span::raw("theirs  mic "),
            on_off(!call.theirs.mic_off),
            Span::raw("  camera "),
            on_off(!call.theirs.camera_off),
            Span::raw("  hold "),
            on_off(call.theirs.held),
        ]),
    ];
    if call.asked_us {
        lines
            .push(Line::styled("they ask to switch to video — y switch, n keep voice", Style::new().fg(Color::Yellow)));
    } else if call.we_asked {
        lines.push(Line::styled("asked them to switch to video (u takes it back)", Style::new().fg(Color::Yellow)));
    }
    lines
}

fn timer(call: &Call) -> String {
    let elapsed = call.connected_at.map(|at| at.elapsed().as_secs()).unwrap_or_default();
    format!("{:02}:{:02}", elapsed / SECONDS_PER_MINUTE, elapsed % SECONDS_PER_MINUTE)
}

fn step(preset: Preset) -> String {
    let video = preset.video();
    format!("{preset:?} {}x{}@{} {} kbps", video.width, video.height, video.fps, video.kbps)
}

fn quality_lines(tui: &Tui) -> Vec<Line<'static>> {
    let network = match tui.network {
        Metered::Wifi => "Wi-Fi",
        Metered::Mobile => "mobile data",
    };
    let cap = tui.cap();
    let mut lines =
        vec![Line::raw(format!("on {network} · cap {} · voice {} kbps", step(cap), cap.voice_bps() / 1000))];
    if let Some(rate) = tui.call.as_ref().and_then(|call| call.rate.as_ref()) {
        lines.push(Line::raw(format!("rate control: step {} · target {} kbps", step(rate.step()), rate.kbps())));
    }
    lines
}

fn traffic_lines(tui: &Tui) -> Vec<Line<'static>> {
    let Some(call) = &tui.call else { return Vec::new() };
    let (Some(stats), Some(live)) = (&call.stats, &call.live) else { return Vec::new() };
    let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let sending: Encoding = *live.encoding.borrow();
    let received: Received = *live.received.borrow();
    let theirs = received.size.map_or_else(|| "?".to_owned(), |(w, h)| format!("{w}x{h}"));
    vec![
        Line::raw(format!(
            "sending   {}x{} · {:.0} kbps {:.1} fps · qp {} · key {} · behind {}",
            sending.width, sending.height, sending.kbps, sending.fps, sending.qp, sending.keyframes, sending.behind
        )),
        Line::raw(format!(
            "          frames sent {} · late {} · congested {} · keyframe asks received {}",
            get(&stats.frames_sent),
            get(&stats.frames_late),
            get(&stats.frames_dropped_congested),
            get(&stats.keyframe_requests_received),
        )),
        Line::raw(format!(
            "receiving {theirs} · {:.0} kbps {:.1} fps · key {} · max {} KiB · turns {} · dropped {} · asks sent {}",
            received.kbps,
            received.fps,
            received.keyframes,
            received.largest_kib,
            received.turns,
            get(&stats.frames_dropped_received),
            get(&stats.keyframe_requests_sent),
        )),
        Line::raw(format!(
            "path      {:?} · rtt {} ms · lost {} of {} datagrams{}",
            stats.route(),
            get(&stats.rtt_ms),
            get(&stats.lost_packets),
            get(&stats.datagrams_sent),
            if stats.stalled() { " · STALLED" } else { "" },
        )),
        Line::raw(format!(
            "voice     sent {} · received {} · late {} · fec {} · concealed {}",
            get(&stats.audio_sent),
            get(&stats.audio_received),
            get(&stats.audio_late),
            get(&stats.audio_fec_recovered),
            get(&stats.audio_concealed),
        )),
    ]
}

fn draw_log(frame: &mut Frame, tui: &Tui, area: Rect) {
    let rows = usize::from(area.height.saturating_sub(2));
    let lines: Vec<Line> = tui
        .log
        .iter()
        .skip(tui.log.len().saturating_sub(rows))
        .map(|line| Line::raw(line.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines).block(Block::bordered().title(" log ")), area);
}

fn keys(tui: &Tui) -> String {
    let common = "w wifi/mobile · [ ] quality · r relays · q quit";
    match tui.call.as_ref().map(|call| (call.phase, call.mode)) {
        None => format!(" ↑↓ pick · Enter/v video call · a voice call · {common}"),
        Some((Phase::Incoming, _)) => format!(" y accept · n reject · {common}"),
        Some((_, Mode::Voice)) => format!(" h hang up · m mic · p hold · u ask video · {common}"),
        Some((_, Mode::Video)) => format!(" h hang up · m mic · c camera · p hold · {common}"),
    }
}
