//! Renders every screen of the app's UI to PNGs with Slint's software renderer.
//!
//! It compiles the app's own `ui.rs`, so what renders here is the markup that ships; only the
//! data is invented. It is layout, spacing and colour that this catches — fonts and safe-area
//! insets are the device's own, and camera frames are stand-ins.

use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::ComponentHandle as _;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

mod compare;
mod dump;

#[path = "../../../crates/uplink/src/ui.rs"]
mod ui;

use ui::{App, Appearance, CallItem, CallState, Confirm, ContactItem, Grant, PermissionItem, Screen, Theme};

/// Logical pixels.
/// The S24 Ultra is 1440x3120 at 3x. `UPLINK_PREVIEW_SIZE=360x799` renders at the design's own
/// frame instead, so its screenshot and this one can be laid over each other.
const WIDTH: u32 = 480;
const HEIGHT: u32 = 1040;
const FRAME_STEP: Duration = Duration::from_millis(16);
/// Long enough for the answer transition (620ms) to finish.
const SETTLE_FRAMES: u32 = 48;
const OUT_DIR: &str = "target/ui-preview";
/// What the app asks for, so the preview's code is drawn at the same scale.
const QR_PIXELS: usize = 512;
/// Fingerprint grouping, as the app formats it.
const GROUP: usize = 4;
const FULL_GROUPS: usize = 8;
const ROW_GROUPS: usize = 4;
const SELF_GROUPS: usize = 3;

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
}

impl slint::platform::Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

/// The canvas to render on, `WIDTHxHEIGHT` from the environment or the device's own.
fn size() -> (u32, u32) {
    let Ok(value) = std::env::var("UPLINK_PREVIEW_SIZE") else {
        return (WIDTH, HEIGHT);
    };
    let parsed = value.split_once('x').and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)));
    parsed.unwrap_or((WIDTH, HEIGHT))
}

fn main() -> Result<()> {
    // `ui-preview <design.png> <build.png>` compares two renders instead of making them.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [left, right] = args.as_slice() {
        return compare::run(left, right);
    }

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless { window: window.clone() }))
        .map_err(|e| anyhow::anyhow!("setting the headless platform: {e}"))?;
    let canvas = size();
    window.set_size(slint::PhysicalSize::new(canvas.0, canvas.1));

    let app = App::new()?;
    populate(&app)?;
    std::fs::create_dir_all(OUT_DIR)?;
    // The app follows the system, and the host has no night mode, so each set says which it is
    // rather than depending on where it runs.
    app.global::<Theme>().set_appearance(Appearance::Dark);

    // What a launch shows first, before anything else is reachable.
    shoot(&window, &app, canvas, "splash")?;
    app.set_booting(false);

    // The gate, first as it is asked and then as it looks once Android has stopped asking.
    app.set_gate(true);
    app.set_permissions(gate(&[Grant::Needed, Grant::Needed, Grant::Needed]));
    shoot(&window, &app, canvas, "permissions")?;
    app.set_permissions(gate(&[Grant::Granted, Grant::Blocked, Grant::Blocked]));
    app.set_permissions_blocked(true);
    shoot(&window, &app, canvas, "permissions-blocked")?;
    app.set_gate(false);

    // Idle screens.
    app.set_call_state(CallState::Idle);
    for (screen, name) in [
        (Screen::People, "people"),
        (Screen::Calls, "calls"),
        (Screen::Connect, "connect"),
        (Screen::Settings, "settings"),
    ] {
        app.set_screen(screen);
        shoot(&window, &app, canvas, name)?;
    }

    // Picking several to remove, and the sheet that asks before anything goes.
    app.set_screen(Screen::People);
    app.set_selecting(true);
    app.set_selected_count(2);
    shoot(&window, &app, canvas, "selecting")?;
    app.set_confirming(Confirm::RemoveSelected);
    shoot(&window, &app, canvas, "confirm")?;
    app.set_confirming(Confirm::None);
    app.set_selecting(false);

    // One contact, opened from People.
    app.set_screen(Screen::People);
    app.set_open_contact_id(keys()[0].1.into());
    app.set_open_contact_name("Noor".into());
    app.set_open_contact_advertised("Noor A.".into());
    app.set_open_contact_initial("N".into());
    app.set_open_contact_favourite(true);
    app.set_open_contact_fingerprint(fingerprint_lines(keys()[0].1));
    shoot(&window, &app, canvas, "contact")?;
    app.set_open_contact_id(Default::default());

    // A key that has arrived and has no name yet, over whatever screen you were on.
    app.set_screen(Screen::People);
    app.set_peer_key("7d192bb40af655c20e62c81291e383bbc63b305338200d8562904d24a46cd641".into());
    shoot(&window, &app, canvas, "name-sheet")?;
    app.set_peer_key(Default::default());

    // People with nobody in it is the first thing a new user sees.
    app.set_screen(Screen::People);
    app.set_contacts(slint::ModelRc::new(slint::VecModel::from(Vec::<ContactItem>::new())));
    shoot(&window, &app, canvas, "people-empty")?;
    populate(&app)?;

    // Scanning takes over the Key screen.
    app.set_screen(Screen::Connect);
    app.set_scanning(true);
    shoot(&window, &app, canvas, "key-scanning")?;
    app.set_scanning(false);

    // Call states.
    for (state, name) in [
        (CallState::Incoming, "call-incoming"),
        (CallState::Dialing, "call-dialing"),
        (CallState::Connected, "call-connected"),
    ] {
        app.set_call_state(state);
        shoot(&window, &app, canvas, name)?;
    }
    app.set_swapped(true);
    shoot(&window, &app, canvas, "call-connected-swapped")?;
    app.set_swapped(false);
    app.set_mic_on(false);
    shoot(&window, &app, canvas, "call-connected-muted")?;

    app.set_call_state(CallState::Idle);
    app.set_log_open(true);
    shoot(&window, &app, canvas, "log")?;
    app.set_log_open(false);

    // The same screens in light. A call is dark in either theme, so it is not repeated here.
    app.global::<Theme>().set_appearance(Appearance::Light);
    for (screen, name) in
        [(Screen::People, "people-light"), (Screen::Connect, "connect-light"), (Screen::Settings, "settings-light")]
    {
        app.set_screen(screen);
        shoot(&window, &app, canvas, name)?;
    }
    app.global::<Theme>().set_appearance(Appearance::System);

    // Not a screen: the picture "share my identity" sends. It is drawn by core rather than by
    // Slint, but it is as much a thing to look at as the rest, and it is not on any screen.
    let card = format!("{OUT_DIR}/identity-card.png");
    std::fs::write(&card, uplink_core::card::identity(keys()[0].1, "Scan to connect")?)?;
    println!("{card}");

    println!("rendered to {OUT_DIR}/");
    Ok(())
}

/// A short name and a long one, so both the common case and eliding are on screen.
const fn keys() -> [(&'static str, &'static str); 3] {
    [
        ("Noor", "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4"),
        ("Ammar", "7d192bb40af655c20e62c81291e383bbc63b305338200d8562904d24a46cd641"),
        ("Laptop in the other room", "e90241d7ba3816fe0e62c81291e383bbc63b305338200d8562904d24a46cd641"),
    ]
}

/// Realistic content: a real QR, stats that wrap, one of every row state.
fn populate(app: &App) -> Result<()> {
    let keys = keys();
    // One favourite, one called recently, one never — the three states a row can be in.
    let rows = [("FAVOURITES", "Called 20 minutes ago", true), ("ALL", "Called Tuesday", false), ("", "Never called", false)];
    let contacts: Vec<ContactItem> = keys
        .iter()
        .zip(rows)
        .map(|((name, key), (header, detail, favourite))| ContactItem {
            name: (*name).into(),
            id: (*key).into(),
            detail: detail.into(),
            header: header.into(),
            initial: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
            tint: 0,
            favourite,
            selected: false,
        })
        .collect();
    app.set_contacts(slint::ModelRc::new(slint::VecModel::from(contacts)));

    // One of each ending, so the screen is reviewed against every state it can show.
    let log = [
        ("Noor", "Missed · 23:04", "TODAY", true, true),
        ("Ammar", "4:12 · 22:15", "", false, false),
        ("Noor", "Cancelled · 19:40", "YESTERDAY", false, false),
        ("7d19 2bb4", "Declined · 11:02", "", false, true),
    ];
    let calls: Vec<CallItem> = log
        .iter()
        .zip(keys.iter().cycle())
        .map(|((name, detail, header, missed, incoming), (_, key))| CallItem {
            initial: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
            name: (*name).into(),
            id: (*key).into(),
            detail: (*detail).into(),
            header: (*header).into(),
            tint: 0,
            missed: *missed,
            incoming: *incoming,
        })
        .collect();
    app.set_calls(slint::ModelRc::new(slint::VecModel::from(calls)));
    app.set_my_id(keys[0].1.into());
    app.set_my_fingerprint_lines(fingerprint_lines(keys[0].1));
    app.set_my_short_fingerprint(groups(keys[0].1, SELF_GROUPS, " · ").into());
    let (qr, mark) = qr_image(keys[0].1)?;
    app.set_qr(qr);
    app.set_qr_mark(mark);
    app.set_peer_name("Noor".into());
    app.set_peer_initial("N".into());
    app.set_call_timer("04:12".into());
    app.set_key_exchange("X25519MLKEM768".into());
    app.set_call_status("".into());
    app.set_frame(stand_in(0x2B, 0x4B, 0x6B));
    app.set_remote_frame(stand_in(0x3A, 0x33, 0x50));
    app.set_stats(
        "cam 30.0 fps · blit 29.8 fps · 640 µs/blit · CPU 18% (100% = 1 core)\n\
         voice ok · sent 1121 received 1030 (late 1, fec 0, concealed 1)\n\
         mic 48000 samples (0 lost) · speaker 48000 samples (0 lost)"
            .into(),
    );
    app.set_log("00:31:02 call connected peer=0e62c812 key_exchange=X25519MLKEM768\n00:31:02 encoder started\n00:31:03 voice streams open rate=48000".into());
    Ok(())
}

/// The gate's three rows in the given states, with the icons the app uses.
fn gate(states: &[Grant; 3]) -> slint::ModelRc<PermissionItem> {
    let rows = [
        ("Camera", "So they can see you", &include_bytes!("../../../assets/icons/camera.svg")[..]),
        ("Microphone", "So they can hear you", &include_bytes!("../../../assets/icons/mic.svg")[..]),
        ("Notifications", "So you know when someone calls", &include_bytes!("../../../assets/icons/bell.svg")[..]),
    ];
    let items: Vec<PermissionItem> = rows
        .iter()
        .zip(states)
        .map(|((name, why, svg), grant)| PermissionItem {
            name: (*name).into(),
            why: (*why).into(),
            grant: *grant,
            icon: slint::Image::load_from_svg_data(svg).unwrap_or_default(),
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// Mirrors the app's own fingerprint formatting; the app keeps its copy next to its contacts.
fn fingerprint_lines(key: &str) -> slint::ModelRc<slint::SharedString> {
    let lines: Vec<slint::SharedString> = key
        .chars()
        .take(GROUP * FULL_GROUPS)
        .collect::<Vec<_>>()
        .chunks(GROUP * ROW_GROUPS)
        .map(|line| {
            line.chunks(GROUP).map(|group| group.iter().collect::<String>()).collect::<Vec<_>>().join(" ").into()
        })
        .collect();
    slint::ModelRc::new(slint::VecModel::from(lines))
}

fn groups(key: &str, count: usize, separator: &str) -> String {
    key.chars()
        .take(GROUP * count)
        .collect::<Vec<_>>()
        .chunks(GROUP)
        .map(|group| group.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(separator)
}

/// The code, and the share of its width the mark in the middle covers.
fn qr_image(key: &str) -> Result<(slint::Image, f32)> {
    let matrix = uplink_core::qr::encode(key)?;
    let (luma, side) = matrix.render(QR_PIXELS);
    let side = u32::try_from(side)?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    for (pixel, value) in buffer.make_mut_slice().iter_mut().zip(luma) {
        *pixel = slint::Rgb8Pixel { r: value, g: value, b: value };
    }
    Ok((slint::Image::from_rgb8(buffer), matrix.logo() as f32 / matrix.framed() as f32))
}

/// A gradient where a camera frame would be, so the call screens aren't reviewed against black.
fn stand_in(r: u8, g: u8, b: u8) -> slint::Image {
    const SIZE: u32 = 240;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(SIZE, SIZE);
    let pixels = buffer.make_mut_slice();
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let index = u32::try_from(index).unwrap_or_default();
        let (x, y) = (index % SIZE, index / SIZE);
        let fade = |channel: u8| {
            let lift = (x + y) * 64 / (SIZE * 2);
            u8::try_from(u32::from(channel) + lift).unwrap_or(u8::MAX)
        };
        *pixel = slint::Rgb8Pixel { r: fade(r), g: fade(g), b: fade(b) };
    }
    slint::Image::from_rgb8(buffer)
}

/// Lets animations and timers settle, then writes one PNG and the element table beside it.
fn shoot(window: &Rc<MinimalSoftwareWindow>, app: &App, canvas: (u32, u32), name: &str) -> Result<()> {
    let (width, height) = canvas;
    let mut buffer = vec![slint::Rgb8Pixel { r: 0, g: 0, b: 0 }; usize::try_from(width * height)?];
    let stride = usize::try_from(width)?;
    for _ in 0..SETTLE_FRAMES {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, stride);
        });
        std::thread::sleep(FRAME_STEP);
    }
    let raw = buffer.iter().flat_map(|pixel| [pixel.r, pixel.g, pixel.b]).collect();
    let image = image::RgbImage::from_raw(width, height, raw).context("frame buffer size")?;
    let path = format!("{OUT_DIR}/{name}.png");
    image.save(&path)?;
    std::fs::write(format!("{OUT_DIR}/{name}.txt"), dump::tree(app.window()))?;
    println!("{path}");
    Ok(())
}
