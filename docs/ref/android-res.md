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
   `AndroidManifest.xml`, `resources.arsc`, the compiled `res/**/*.xml` and the copied `res/**/*.png`.
2. `android-res package --dir <stage> --out <apk>` zips it.
3. `apksigner` (a jar, so it runs here) signs it.

## Sources live in `android/`

Real XML, not element trees in Rust — change the manifest without recompiling a tool.

```
android/AndroidManifest.xml      ${package}, ${label}, ${minSdk}… from --define
android/res/values/values.xml    <color>, <style>, <string>, <plurals> → resources.arsc
android/res/values-ar/values.xml Arabic <string>/<plurals>: a second, locale-tagged table
android/res/mipmap/launcher.xml  <adaptive-icon>, compiled to binary XML
android/res/drawable/*.png       copied, referenced by name
```

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

`tools/design-probe/mark.py` renders the u-link to `android/res/drawable/mark.png` and
`mark_mono.png` with headless Chromium: 240px of mark centred on a transparent 432px canvas, which
is 108dp at xxxhdpi with the mark inside the 72dp the launcher's mask always keeps. The monochrome
layer is the same path in flat white, because the system tints it.

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
