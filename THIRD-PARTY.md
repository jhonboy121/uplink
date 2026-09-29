# Third-party software

uplink's own code is licensed under MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`). It is built on
the components below, each under its own license. Anyone distributing a build (an APK, a CLI binary)
must pass these notices on with it, along with those of every crate in `Cargo.lock`.

## Slint

The UI toolkit. Licensed as `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR
LicenseRef-Slint-Software-3.0`; uplink uses it under the **Slint Royalty-free Desktop, Mobile, and Web
Applications License 2.0**. That license's attribution condition is met by the "Made with Slint" badge in
`README.md`. A fork that takes the badge out must add Slint's `AboutSlint` widget to an About screen
instead, or use Slint under GPL-3.0. The license does not cover embedded systems.
Terms: <https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md>

## Submodules

| Component | Where | License | Notice |
|---|---|---|---|
| opusorus, a Rust port of libopus | `external/opusorus` | BSD-3-Clause | libopus copyright holders, in `external/opusorus/LICENSE` |
| h264, a Rust port of OpenH264 (CLI only) | `external/h264` | BSD-2-Clause | Cisco's OpenH264 notice, in `external/h264/LICENSE` |
| ndk, ndk-sys | `external/ndk` | MIT OR Apache-2.0 | `external/ndk/LICENSE-*` |
| ndk-context | `external/ndk-context` | MIT OR Apache-2.0 | `external/ndk-context/LICENSE-*` |
| netwatch (n0) | `external/net-tools/netwatch` | MIT OR Apache-2.0 | `external/net-tools/LICENSE-*` |
| ndk-gl-sys bindings | `sys/ndk-gl-sys` | generated from the Android NDK's headers (Apache-2.0) | |

## Fonts

Bundled into the app, all under the **SIL Open Font License 1.1**. Each license sits beside its fonts in
`assets/fonts`:

| Font | License file |
|---|---|
| IBM Plex Mono | `OFL-IBMPlexMono.txt` |
| Noto Sans Arabic | `OFL-NotoSansArabic.txt` |
| Outfit | `OFL-Outfit.txt` |
| Public Sans | `OFL-PublicSans.txt` |

## Crates with conditions beyond MIT/Apache-2.0

Most of the several hundred crates in the tree are MIT and/or Apache-2.0. These are not:

| Crate(s) | License | Note |
|---|---|---|
| symphonia (CLI only) | MPL-2.0 | File-level copyleft; used unmodified from crates.io |
| curve25519-dalek, ed25519-dalek, and others | BSD-3-Clause | |
| aws-lc-rs, aws-lc-sys, ring | ISC, Apache-2.0, MIT, BSD-3-Clause (combined) | See each crate's license files |
| rustls-webpki, untrusted, libloading | ISC | |
| webpki-roots | CDLA-Permissive-2.0 | Mozilla's root certificate list |
| icu_* | Unicode-3.0 | |
| rav1e, av1-grain, and others | BSD-2-Clause | |
| zlib-rs, foldhash, slotmap | Zlib | |
| terminfo (CLI only) | WTFPL | |

To list every crate and its license: `cargo metadata --format-version 1` (the `license` field of each
package), or a tool such as `cargo-about`, which also collects the license texts.
