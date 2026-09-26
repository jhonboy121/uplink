#!/usr/bin/env python3
"""Renders the u-link into the PNG badge the QR code carries, which Rust draws into an image.

Android's own copies (the launcher layers and the notification icon) are `<vector>` drawables
now, written by `just drawables` from the same SVG; this is only the badge.

The badge is the same mark for the middle of a QR code, where it sits on a white plate rather
than on the app's ground. It gets its own render because the art is not centred in the 48-unit
viewBox — it runs from y 4.4 to y 38.75 — so a square crop of the viewBox would hang the mark
high in its plate. `ART` is that bounding box, and the browser centres it for us.
"""

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
ICONS = ROOT / "assets" / "icons"
# Wherever this box keeps it (a snap on Ubuntu); `CHROMIUM=` names another.
CHROMIUM = os.environ.get("CHROMIUM") or shutil.which("chromium") or "chromium"
# The badge is small on screen (a fifth of a code) but is also drawn into a shared image at a
# size we do not control, so it is rendered well above either.
BADGE = 256
# The mark's own bounds inside the viewBox: the circles' outer edges and the arc's stroke.
ART = "8.4 4.4 31.2 34.35"

PAGE = """<!doctype html>
<style>
  html, body {{ margin: 0; background: transparent; }}
  body {{ width: {canvas}px; height: {canvas}px; display: grid; place-items: center; }}
</style>
<svg width="{mark}" height="{mark}" viewBox="{box}" fill="none">
  <path d="M13 18 V25 A11 11 0 0 0 35 25 V18" stroke="{accent}" stroke-width="5.5" stroke-linecap="round"/>
  <circle cx="13" cy="9" r="4.6" fill="{peer}"/>
  <circle cx="35" cy="9" r="4.6" fill="{peer}"/>
</svg>
"""


def render(out, accent, peer, canvas, mark, box):
    out.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as work:
        page = pathlib.Path(work) / "mark.html"
        page.write_text(PAGE.format(canvas=canvas, mark=mark, box=box, accent=accent, peer=peer))
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
                f"--window-size={canvas},{canvas}",
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


# On a white plate, so the mark takes the light theme's colours.
render(ICONS / "mark-badge.png", "#0A6FC2", "#0E151E", canvas=BADGE, mark=BADGE, box=ART)
