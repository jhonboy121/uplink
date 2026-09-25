# Matching the build to the locked design

The design is the specification. Where the build differs, **the build changes** — do not write a
rationale for a divergence. An earlier session marked nine real gaps "closed" by explaining why the
build's choice was better; all nine were wrong and had to be undone.

Read this before touching the markup in `crates/uplink/ui/`.

## The two sources of truth

| | Where | Command |
|---|---|---|
| Design | `docs/design/uplink-call-ui.html` (the locked mockup, vendored from the artifact) | `just design-measure`, `just design-shot <Screen>` |
| Build | `crates/uplink/ui/*.slint` (via `src/ui.rs`), compiled by `tools/ui-preview` | `just preview` |
| Both | — | `just ui-diff <build-name> <Screen>` |

Screens are named by the mockup's own captions: `People`, `Add someone`, `Settings`, `In a call`,
`Incoming`, `العربية`.

- The shell (People rows, contact and call pages, the call log) has its own mockup,
  `docs/design/uplink-shell.html`. Measure it with
  `UPLINK_DESIGN=uplink-shell` before `python3 tools/design-probe/probe.py` and `table.py`; it writes
  `geometry-uplink-shell.json`. Where two frames share a caption, the later one is what gets measured.
  Its older rows differ from the built ones by a few dp; only what a change touches is matched to it.
- `just design-measure` → `target/design/geometry.json`: every element of every screen, measured in
  headless Chromium. Positions, sizes, computed font/colour/border/padding/gap, and the glyph run.
- `just design-table <Screen>` prints one screen scaled to dp, in the same column layout as the
  build's tables.
- `just preview` → `target/ui-preview/<screen>.png` **and `.txt`**, the element table straight out
  of Slint's item tree. `UPLINK_PREVIEW_SIZE=360x772` renders at the design's own frame.
- `just ui-diff` screenshots the design, renders the build at the same size, and compares them by
  bands of ink — which works while one is light and the other dark.

## The scale: the mockup is a 360dp phone

The mockup's phone frame is a **248 CSS px** screen. It stands for a **360dp** phone, so every
length in it is multiplied by **360/248 = 1.4516** to reach the dp the build is written in. This was
settled by checking three ways: scaled to 360 it lands on the type table's roles (23.2 ≈
headline-small, 12.3 ≈ the 13px fingerprint) and on Material's 72dp two-line row; scaled to the
S24's 480dp everything came out a third too big.

The preview canvas is 480×1040 because that is the S24 Ultra (1440×3120 at 3x). **A wider phone
shows more rows, not bigger ones** — never rescale the design to the canvas.

## What burned time, in order of cost

1. **Comparing two pictures by eye.** It is how the "closed" verdicts happened. Use the tables.
2. **Reading the CSS instead of measuring it.** Declared values are not resolved values. Every font
   size derived by arithmetic was wrong by a point: the title is 23, not 24; the name 17, not 16.
3. **Line boxes.** The design sets `line-height: 1.6`. A 17dp name occupies a **28dp** box, and the
   name+fingerprint block is 49dp tall; Slint's text box is the glyphs' own height, 19dp, giving 37.
   That 12dp was most of what looked wrong. Fix: `height: self.font-size * Theme.line-box;` plus
   `vertical-alignment: center;` on any text whose position matters.
4. **Element boxes are not glyph runs.** A `Text` in a layout reports its *stretched* width (317dp),
   never the width the letters occupy. Both tables now print `ink=` / `ink=WxH`; compare those.
5. **Fractional font sizes.** The design's sizes are not whole dp — the fingerprint is 8.5px ×
   1.4516 = **12.34**. Rounding to 12 cost 5dp across a 25-character run. Use `0.771rem`.
6. **`font-family: "monospace"` does not resolve** in the software renderer. Three equal-length
   strings rendered 155/152/153 wide. Name the real face: `Theme.mono`.
7. **A fixed-size child of a Slint `HorizontalLayout` is not centred.** The avatar sat at the row's
   top edge. Wrap it: `VerticalLayout { alignment: center; ... }`.
8. **Colour is invisible to the tables.** The avatar tint order and the per-tint initial colour were
   only ever caught by looking at the two PNGs. Do both: tables for geometry, images for colour.

## Tooling gotchas

- `SLINT_EMIT_DEBUG_INFO=1` is required for element names in the build's table; `just preview` sets
  it. Without it the tables are anonymous.
- `ItemRc::map_to_window` accumulates the **ancestors'** offsets but not the item's own, so map
  `geometry().origin`, not `(0,0)`. Getting this wrong reported the tab bar at `y=0`.
- The probe must `await document.fonts.ready` before measuring. Measured earlier, every text box is
  the fallback's height and the whole rhythm is wrong.
- The design's `#E7ECF2` separator is only 24/255 off its white ground, so the ink threshold in
  `tools/ui-preview/src/compare.rs` has to stay low (10) or hairlines vanish from the band list.
- The band diff finds **position, not identity**. It would pass two screens whose rows were in the
  wrong order. It is a check, not a proof.
- A constant `dx -2` between the two images is antialiasing (a pale circle on white loses its edge
  pixel, the same circle on dark does not), not a layout difference. Check the right edges.

## Refactoring without moving anything

A change that should not move anything (a split, a new shared component) is checked by diffing
the element tables of `just preview` before and after: keep the painted rows only (text, fill,
image) and ignore element names, which a refactor changes. The 2026-09-24 split into `ui/*.slint`
and the move to a flexbox `Row` came out identical but for **1px** shifts: a text centred at
y=105.5 used to be truncated to 105 and taffy (the flexbox solver) rounds it to 106. That is
sub-pixel, not a layout change. Anything over 1px is real.

## Assets

Everything lives in `assets/`, addressed through one `#[include_path = "../../../assets"]` at the
top of the `slint!` macro — never a relative path repeated at each use. Slint resolves an include
path against *each importing file's own folder*, so `crates/uplink/ui/` stays flat and as deep as
`src/`: a `.slint` file one folder deeper would look for `crates/assets`.

- `assets/fonts/` — Outfit (400/600), Public Sans (400/600/700), IBM Plex Mono (400/500) as static
  per-weight TTFs, with their OFL licences. Google Fonts serves only variable fonts now, so these
  come from upstream. Registered by `import "fonts/X.ttf";` in the markup; no build script.
- `assets/icons/` — the SVGs, used as `@image-url("icons/close.svg")`.

Roles: **Outfit** names things (titles, contact names, avatar initials), **Public Sans** is the
window default for prose, **IBM Plex Mono** is anything read character by character — keys,
fingerprints, the call timer.

## What is deliberately not the design

- **The call screen is always dark**, whatever the system theme. Lists and settings follow it.
- Test data differs on purpose: the design's third contact is "Laptop", the preview's is "Laptop in
  the other room", so eliding is visible.
- The preview is the host's software renderer. Skia on the device hints differently and the camera
  frames are stand-ins; the phone gets the last word.

## Still open

- The design draws no route to Settings and no way to remove a contact. Both are unresolved, not
  forgotten — ask before inventing one.
- Verification ("verified key" chip) is not built, so the app must not claim it.
