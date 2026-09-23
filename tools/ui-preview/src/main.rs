//! Renders every screen of the app's UI to PNGs with Slint's software renderer.
//!
//! It compiles the app's own `ui.rs`, so what renders here is the markup that ships; only the
//! data is invented. It is layout, spacing and colour that this catches — fonts and safe-area
//! insets are the device's own, and camera frames are stand-ins.

use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};

#[path = "../../../crates/uplink/src/ui.rs"]
mod ui;

use ui::{App, CallState, ContactItem, Screen};

/// Logical pixels: an S24 Ultra is 1440x3120 at 3x.
const WIDTH: u32 = 480;
const HEIGHT: u32 = 1040;
const FRAME_STEP: Duration = Duration::from_millis(16);
/// Long enough for the answer transition (620ms) to finish.
const SETTLE_FRAMES: u32 = 48;
const OUT_DIR: &str = "target/ui-preview";

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
}

impl slint::platform::Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn main() -> Result<()> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless { window: window.clone() }))
        .map_err(|e| anyhow::anyhow!("setting the headless platform: {e}"))?;
    window.set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));

    let app = App::new()?;
    populate(&app)?;
    std::fs::create_dir_all(OUT_DIR)?;

    // Idle screens.
    app.set_call_state(CallState::Idle);
    for (screen, name) in [(Screen::People, "people"), (Screen::Identity, "key"), (Screen::Settings, "settings")] {
        app.set_screen(screen);
        shoot(&window, name)?;
    }

    // The Key screen once a code has been read: all that is left is naming them.
    app.set_screen(Screen::Identity);
    app.set_peer_key("7d192bb40af655c20e62c81291e383bbc63b305338200d8562904d24a46cd641".into());
    shoot(&window, "key-pending")?;
    app.set_peer_key(Default::default());

    // People with nobody in it is the first thing a new user sees.
    app.set_screen(Screen::People);
    app.set_contacts(slint::ModelRc::new(slint::VecModel::from(Vec::<ContactItem>::new())));
    shoot(&window, "people-empty")?;
    populate(&app)?;

    // Scanning takes over the Key screen.
    app.set_screen(Screen::Identity);
    app.set_scanning(true);
    shoot(&window, "key-scanning")?;
    app.set_scanning(false);

    // Call states.
    for (state, name) in [
        (CallState::Incoming, "call-incoming"),
        (CallState::Dialing, "call-dialing"),
        (CallState::Connected, "call-connected"),
    ] {
        app.set_call_state(state);
        shoot(&window, name)?;
    }
    app.set_swapped(true);
    shoot(&window, "call-connected-swapped")?;
    app.set_swapped(false);
    app.set_mic_on(false);
    shoot(&window, "call-connected-muted")?;

    app.set_call_state(CallState::Idle);
    app.set_log_open(true);
    shoot(&window, "log")?;

    println!("rendered to {OUT_DIR}/");
    Ok(())
}

/// Realistic content: a short name and a long one, a real QR, stats that wrap.
fn populate(app: &App) -> Result<()> {
    let keys = [
        ("Noor", "0e62c81291e383bbc63b305338200d8562904d24a46cd6412f9bae0c7ae111f4"),
        ("Ammar", "7d192bb40af655c20e62c81291e383bbc63b305338200d8562904d24a46cd641"),
        ("Laptop in the other room", "e90241d7ba3816fe0e62c81291e383bbc63b305338200d8562904d24a46cd641"),
    ];
    let contacts: Vec<ContactItem> = keys
        .iter()
        .map(|(name, key)| ContactItem {
            name: (*name).into(),
            id: (*key).into(),
            fingerprint: fingerprint(key).into(),
            initial: name.chars().next().unwrap_or('?').to_uppercase().to_string().into(),
            tint: 0,
        })
        .collect();
    app.set_contacts(slint::ModelRc::new(slint::VecModel::from(contacts)));
    app.set_my_id(keys[0].1.into());
    app.set_my_fingerprint(fingerprint(keys[0].1).into());
    app.set_qr(qr_image(keys[0].1)?);
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

fn fingerprint(key: &str) -> String {
    key.chars().take(32).collect::<Vec<_>>().chunks(4).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(" ")
}

fn qr_image(key: &str) -> Result<slint::Image> {
    let (luma, side) = uplink_core::qr::render(key, 512)?;
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(side, side);
    for (pixel, value) in buffer.make_mut_slice().iter_mut().zip(luma) {
        *pixel = slint::Rgb8Pixel { r: value, g: value, b: value };
    }
    Ok(slint::Image::from_rgb8(buffer))
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

/// Lets animations and timers settle, then writes one PNG.
fn shoot(window: &Rc<MinimalSoftwareWindow>, name: &str) -> Result<()> {
    let mut buffer = vec![slint::Rgb8Pixel { r: 0, g: 0, b: 0 }; usize::try_from(WIDTH * HEIGHT)?];
    let stride = usize::try_from(WIDTH)?;
    for _ in 0..SETTLE_FRAMES {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, stride);
        });
        std::thread::sleep(FRAME_STEP);
    }
    let raw = buffer.iter().flat_map(|pixel| [pixel.r, pixel.g, pixel.b]).collect();
    let image = image::RgbImage::from_raw(WIDTH, HEIGHT, raw).context("frame buffer size")?;
    let path = format!("{OUT_DIR}/{name}.png");
    image.save(&path)?;
    println!("{path}");
    Ok(())
}
