#!/usr/bin/env python3
"""Renders the u-link into the PNGs the adaptive icon references.

An adaptive icon's layers are 108dp but only the middle 72dp always survives the launcher's mask,
so the mark is drawn at 60dp on a transparent 432px canvas (108dp at xxxhdpi). The monochrome
layer is the same shape in one colour, because a themed icon is tinted by the system.
"""

import pathlib
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT = ROOT / "android" / "res" / "drawable"
CHROMIUM = "/usr/bin/chromium"
# 108dp at xxxhdpi, and the safe circle the mask keeps.
CANVAS = 432
MARK = 240

PAGE = """<!doctype html>
<style>
  html, body {{ margin: 0; background: transparent; }}
  body {{ width: {canvas}px; height: {canvas}px; display: grid; place-items: center; }}
</style>
<svg width="{mark}" height="{mark}" viewBox="0 0 48 48" fill="none">
  <path d="M13 18 V25 A11 11 0 0 0 35 25 V18" stroke="{accent}" stroke-width="5.5" stroke-linecap="round"/>
  <circle cx="13" cy="9" r="4.6" fill="{peer}"/>
  <circle cx="35" cy="9" r="4.6" fill="{peer}"/>
</svg>
"""


def render(name, accent, peer):
    out = OUT / f"{name}.png"
    with tempfile.TemporaryDirectory() as work:
        page = pathlib.Path(work) / "mark.html"
        page.write_text(PAGE.format(canvas=CANVAS, mark=MARK, accent=accent, peer=peer))
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
                f"--window-size={CANVAS},{CANVAS}",
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
# The launcher's own layer keeps the palette; the themed one is tinted, so it is drawn flat.
render("mark", "#58B6FF", "#E9F0F7")
render("mark_mono", "#FFFFFF", "#FFFFFF")
