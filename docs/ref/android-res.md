# Building an APK without aapt2

`tools/android-res` does what aapt2 and zipalign do for us, because **both ship as x86-64 binaries
only** and this machine is arm64:

```
build-tools/37.0.0/aapt2:    ELF 64-bit LSB pie executable, x86-64
build-tools/37.0.0/zipalign: ELF 64-bit LSB pie executable, x86-64
```

They are also linked against glibc, so running them under `qemu-x86_64` would need an x86-64 glibc
sysroot as well. Writing the formats was cheaper and has no runtime dependency.

## The pipeline

`just apk` stages a directory, then:

1. `android-res compile --out <stage> --define …` reads `android/` and writes the binary
   `AndroidManifest.xml`, `resources.arsc` and the compiled `res/**/*.xml`.
2. `android-res package --dir <stage> --out <apk>` zips it.
3. `apksigner` (a jar, so it runs here) signs it.

## Sources live in `android/`

Real XML, not element trees in Rust — change the manifest without recompiling a tool.

```
android/AndroidManifest.xml      the app's own values written in it; --define only for what the
                                 build uses too (${package} ${activity} ${lib} ${minSdk}) or
                                 what differs per build (${debuggable})
android/res/values/values.xml    <color>, <style>, <string>, <plurals> → resources.arsc
android/res/values-ar/values.xml Arabic <string>/<plurals>: a second, locale-tagged table
android/res/mipmap/launcher.xml  <adaptive-icon>, compiled to binary XML
android/res/drawable/*.xml       <vector>s generated from assets/icons (just drawables)
```

Flag and enum attributes are written as the SDK's words, `foregroundServiceType="phoneCall|camera"`,
`launchMode="singleTop"`. The words and their numbers come from `ActivityInfo`, `ServiceInfo` and
`WindowManager.LayoutParams` through the same `just android-table` javap run as the attribute ids
(`framework::VALUE_NAMES`); flags are written in hex and enums in decimal, as aapt2 does. A bare
number still compiles, for a value no word covers.

Names resolve two ways: `android:foo` through the **generated** framework table
(`framework.rs`, `just android-table`, 1560 attrs + 737 styles from `android.jar` via `javap`), and
`@color/ground` through a symbol table built from `res/` before anything compiles.

**Never hand-copy a framework id.** Generating the table immediately caught one that had been typed
from memory: `Theme.DeviceDefault.DayNight` is `0x010302e3`, not `0x01030345`. Nothing would have
failed loudly — the theme would simply have inherited from the wrong style.

## Two rules the format imposes

- **API 30+ refuses an APK whose `resources.arsc` is compressed or unaligned.** It must be stored
  (method 0) and its data must begin on a 4-byte boundary. `jar` deflates everything, which is why
  `android-res package` exists: it deflates every entry except the table, and pads the table's
  extra field (id `0xd935`, as zipalign uses) to align it. apksigner copies entry data verbatim, so
  the alignment survives signing.
- **minSdk 30 means adaptive icons are always available.** So there is one `mipmap` entry in the
  default configuration — no density buckets, no `-v26` qualifier, no legacy square fallback.

## Verifying without a device

`cmdline-tools/latest/bin/apkanalyzer` is **Java**, so unlike aapt2 it runs here. It is the way to
check output before flashing anything:

```
apkanalyzer manifest print app.apk
apkanalyzer resources names --type style --config default app.apk
apkanalyzer resources value --type color --name ground --config default app.apk
apkanalyzer resources xml --file res/mipmap/launcher.xml app.apk
```

## The icon

Every drawable is a `<vector>`, not a PNG: `just drawables` (`android-res vectors`, in
`src/vector.rs`) writes them from `assets/icons/*.svg`, the same files the window draws, as
elements and attributes through `xmlwriter` (never string-built). The results are checked in;
regenerate after changing an icon. The launcher layers put the mark at 60dp in the middle of a 108dp
layer, inside the 72dp the mask always keeps; `mark_mono` is the same shape in white, because the
system tints it; `notification` is the mark cropped to its own bounds, in white, because a status-bar
icon is drawn from its alpha. Only `<path>` and `<circle>` are understood — anything else is an
error. (`tools/design-probe/mark.py` now renders only the QR badge, which Rust draws into an image.)

`<vector>` needs value types plain XML cannot tell from the text, so `compile.rs` types these by
attribute name, as aapt2 does from the SDK's `attrs.xml`: colours (`fillColor`, `strokeColor`,
`tint` → `TYPE_INT_COLOR_ARGB8`), floats (`viewportWidth`, `strokeWidth`, the group transforms →
`TYPE_FLOAT`), whole `dp` (`width`, `height` → `TYPE_DIMENSION`, `n << 8 | 1`), and the enums
`strokeLineCap`, `strokeLineJoin`, `fillType`. A colour left as a string would fail at inflation
(`getComplexColor` reads a string as a file path). Check a compiled one with
`apkanalyzer resources xml --file res/drawable/mic.xml app.apk`.

The splash needs no `values-v31` work: Android 12+ builds its own from `android:icon` and the
theme's `windowBackground`, so setting those to the mark and to `@color/ground` makes the system
splash and our in-app one match. On API 30 the same `windowBackground` paints the window before the
first frame instead of flashing white.

## Strings, languages and `R` ✅ (2026-09-24)

- `<string>` and `<plurals>` compile into the `string` and `plurals` types. New types are appended
  to `TYPES`, so existing ids never move.
- **A language is a second type chunk** for the same type: `ResTable_config` with only its
  `language` bytes set (offset 8, e.g. `ar`), entries aligned with the default by index and
  `0xFFFFFFFF` where there is no translation (the lookup falls back to the default). The type
  spec flags each translated entry with `CONFIG_LOCALE` (`0x0004`).
- **A plural is a map entry** with no parent, keyed by `ResTable_map`'s quantity names:
  `ATTR_OTHER` … `ATTR_MANY` = `0x01000004` … `0x01000009`. `other` is required.
- A string's text is read as aapt2 does: whitespace folds, `\'` `\"` `\\` `\n` `\t` `\@` `\?`
  unescape, and a leading `@`/`?` must be escaped.
- The string pool's first length is **UTF-16 units**, not chars.
- Verified with `apkanalyzer resources configs|value --config ar` — the Arabic table, the six
  Arabic plural forms, and the colour/style ids unchanged.
- **`android-res r-class --package dev.uplink --out <dir>`** writes `R.java` from the same symbol
  table `compile` uses (`just dex` generates it into a temp dir and compiles it with the rest),
  so Java writes `R.string.mute` / `R.drawable.call_end` instead of `getIdentifier` by name.
- The app's language, not the phone's: Rust calls `UplinkApplication.setLanguage(code)` on every
  core start and on a change; Java keeps it in SharedPreferences (a boot posts before the core is
  up) and reads every string through `createConfigurationContext` with that locale.
