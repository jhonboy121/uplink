#!/usr/bin/env python3
"""Renders the call icons into PNG drawables for the picture-in-picture window's buttons.

Android's `RemoteAction` takes an `Icon`, which means a real drawable in the resource table — it
cannot read the SVGs the UI uses. These are the same files, rasterised white on transparent at a
size the system scales down rather than up.
"""

import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
ICONS = ROOT / "assets" / "icons"
OUT = ROOT / "android" / "res" / "drawable"
CHROMIUM = "/usr/bin/chromium"
# Comfortably above the ~48dp the system draws them at, on any density.
SIZE = 144
# The SVG name, and the drawable name UplinkActivity looks up with getIdentifier.
WANTED = {
    "mic.svg": "mic",
    "mic-off.svg": "mic_off",
    "call-end.svg": "call_end",
    # The output button, which draws where the sound goes now.
    "call.svg": "output_phone",
    "speaker.svg": "output_speaker",
    "bluetooth.svg": "output_bluetooth",
    "headphones.svg": "output_wired",
    "speaker-off.svg": "output_mute",
}

PAGE = """<!doctype html>
<style>
  html, body {{ margin: 0; background: transparent; }}
  body {{ width: {size}px; height: {size}px; display: grid; place-items: center; }}
  svg {{ width: {size}px; height: {size}px; }}
</style>
{svg}
"""


def render(source, name):
    out = OUT / f"{name}.png"
    with tempfile.TemporaryDirectory() as work:
        page = pathlib.Path(work) / "icon.html"
        page.write_text(PAGE.format(size=SIZE, svg=source.read_text()))
        subprocess.run(
            [
                CHROMIUM,
                "--headless",
                "--no-sandbox",
                "--disable-gpu",
                f"--user-data-dir={work}/profile",
                "--default-background-color=00000000",
                "--hide-scrollbars",
                f"--screenshot={out}",
                f"--window-size={SIZE},{SIZE}",
                page.as_uri(),
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=120,
            check=False,
        )
    if not out.exists():
        sys.exit(f"chromium did not write {out}")
    print(f"{out} ({out.stat().st_size} bytes)")


OUT.mkdir(parents=True, exist_ok=True)
for svg, name in WANTED.items():
    render(ICONS / svg, name)
