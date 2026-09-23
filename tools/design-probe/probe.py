#!/usr/bin/env python3
"""Measures the design mockup in a real browser and writes geometry.json.

Headless Chromium loads the page from a local server; the page measures itself once the webfonts
have loaded and posts the numbers back. The browser is the only thing here that knows what the
CSS actually resolves to — margin collapsing, line boxes, flex gaps and all.
"""

import json
import pathlib
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[2]
PAGE = ROOT / "docs" / "design" / "uplink-call-ui.html"
OUT = ROOT / "target" / "design" / "geometry.json"
PORT = 8173
TIMEOUT = 120
CHROMIUM = "/usr/bin/chromium"
# The mockup's phone: a 248px screen standing for a 360dp one, at the frame's own 9/19.3.
FRAME_PX = 248
TARGET_DP = 360
ASPECT = 19.3 / 9

# The frame is pinned so the measurement does not depend on the window: the mockup's phone is
# 264px with 7px padding and a 1px border, leaving a 248px screen.
PROBE = """
<style>figure.frame .phone { width: 264px !important; max-width: none !important; }</style>
<script>
(async () => {
  try { await document.fonts.ready; } catch (e) {}
  await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));

  const px = v => Math.round(parseFloat(v) * 100) / 100 || 0;
  const screens = {};

  for (const figure of document.querySelectorAll('figure.frame')) {
    const screen = figure.querySelector('.screen');
    const caption = figure.querySelector('figcaption b');
    if (!screen || !caption) continue;
    const origin = screen.getBoundingClientRect();
    const items = [];

    const walk = (node, depth) => {
      const box = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      // The element's own text, not its descendants'.
      const own = [...node.childNodes]
        .filter(n => n.nodeType === 3)
        .map(n => n.textContent.trim())
        .join(' ')
        .trim();
      // A text node's own box: what the glyphs occupy, which is not the element's box once a
      // layout has stretched it. This is the only way to compare a string's rendered width.
      let ink = null;
      if (own) {
        const range = document.createRange();
        range.selectNodeContents(node);
        const r = range.getBoundingClientRect();
        ink = { x: px(r.left - origin.left), y: px(r.top - origin.top), w: px(r.width), h: px(r.height) };
      }
      items.push({
        ink,
        tracking: style.letterSpacing === 'normal' ? '' : style.letterSpacing,
        wordSpacing: style.wordSpacing === 'normal' ? '' : style.wordSpacing,
        depth,
        tag: node.tagName.toLowerCase(),
        cls: typeof node.className === 'string' ? node.className : '',
        text: own.slice(0, 40),
        x: px(box.left - origin.left),
        y: px(box.top - origin.top),
        w: px(box.width),
        h: px(box.height),
        font: px(style.fontSize),
        weight: style.fontWeight,
        family: (style.fontFamily || '').split(',')[0].replace(/["']/g, ''),
        color: style.color,
        bg: style.backgroundColor,
        radius: style.borderTopLeftRadius,
        border: px(style.borderTopWidth) ? style.borderTopWidth + ' ' + style.borderTopColor : '',
        display: style.display,
        gap: style.rowGap === 'normal' ? '' : style.rowGap + '/' + style.columnGap,
        pad: [style.paddingTop, style.paddingRight, style.paddingBottom, style.paddingLeft].join(' '),
        align: style.alignItems,
      });
      for (const child of node.children) walk(child, depth + 1);
    };

    walk(screen, 0);
    screens[caption.textContent.trim()] = { width: px(origin.width), height: px(origin.height), items };
  }

  await fetch('/geometry', { method: 'POST', body: JSON.stringify({ screens }, null, 1) });
})();
</script>
"""

DONE = threading.Event()

# One screen alone on a white page at its own size, so Chromium's screenshot is exactly that
# screen and nothing else. `?shot=<name>` selects it by the caption the mockup gives it.
SHOT = """
<style>
  body { margin: 0 !important; background: #fff !important; }
  .wrap > *:not(#screens), nav.jump, header.top, .section-head, .demo-controls { display: none !important; }
  .wrap { padding: 0 !important; max-width: none !important; }
  .screens { display: block !important; gap: 0 !important; }
  figure.frame { display: none; margin: 0 !important; gap: 0 !important; }
  figure.frame.shot { display: block !important; }
  figure.frame.shot figcaption { display: none !important; }
  figure.frame.shot .phone {
    width: 248px !important; max-width: none !important;
    margin: 0 !important; padding: 0 !important;
    border: 0 !important; border-radius: 0 !important; box-shadow: none !important;
  }
  figure.frame.shot .screen { border-radius: 0 !important; }
  /* Android draws the status bar, not the app, so the design's stand-in comes off. */
  figure.frame.shot .statusbar { display: none !important; }
</style>
<script>
(async () => {
  const want = new URLSearchParams(location.search).get('shot');
  for (const figure of document.querySelectorAll('figure.frame')) {
    const caption = figure.querySelector('figcaption b');
    if (caption && caption.textContent.trim() === want) figure.classList.add('shot');
  }
  try { await document.fonts.ready; } catch (e) {}
})();
</script>
"""


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        page = PAGE.read_text(encoding="utf-8")
        if self.path.startswith("/shot"):
            body = (page + SHOT).encode("utf-8")
        elif self.path == "/":
            body = (page + PROBE).encode("utf-8")
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        payload = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        OUT.write_bytes(payload)
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")
        DONE.set()

    def log_message(self, *args):
        pass


def chromium(profile, *args):
    return [
        CHROMIUM,
        "--headless",
        "--no-sandbox",
        "--disable-gpu",
        "--disable-dev-shm-usage",
        f"--user-data-dir={profile}",
        *args,
    ]


def screenshot(screen):
    """One screen as a PNG at the device scale, so it overlays the build's render directly."""
    out = OUT.parent / (screen.lower().replace(" ", "-") + ".png")
    with tempfile.TemporaryDirectory() as profile:
        subprocess.run(
            chromium(
                profile,
                f"--screenshot={out}",
                f"--window-size={FRAME_PX},{round(FRAME_PX * ASPECT)}",
                f"--force-device-scale-factor={TARGET_DP / FRAME_PX}",
                "--hide-scrollbars",
                "--virtual-time-budget=8000",
                f"http://127.0.0.1:{PORT}/shot?shot={screen}",
            ),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=TIMEOUT,
            check=False,
        )
    return out


def main():
    if not PAGE.exists():
        sys.exit(f"missing {PAGE}")
    OUT.parent.mkdir(parents=True, exist_ok=True)
    server = HTTPServer(("127.0.0.1", PORT), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    if len(sys.argv) > 1:
        for screen in sys.argv[1:]:
            path = screenshot(screen)
            print(f"{path} ({'ok' if path.exists() else 'FAILED'})")
        return

    with tempfile.TemporaryDirectory() as profile:
        browser = subprocess.Popen(
            chromium(profile, "--window-size=1600,2400", f"http://127.0.0.1:{PORT}/"),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        measured = DONE.wait(TIMEOUT)
        browser.terminate()
        try:
            browser.wait(10)
        except subprocess.TimeoutExpired:
            browser.kill()

    if not measured:
        sys.exit(f"no measurements after {TIMEOUT}s")
    screens = json.loads(OUT.read_bytes())["screens"]
    counts = ", ".join(f"{name} ({len(data['items'])})" for name, data in screens.items())
    print(f"wrote {OUT}\nscreens: {counts}")


main()
