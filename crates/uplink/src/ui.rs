//! The Slint UI, whose markup lives in `crates/uplink/ui/`: `app.slint` is the window, and it
//! re-exports every type Rust sees. Strings are `@tr`'d there and translated from
//! `crates/uplink/lang/` (see `.cargo/config.toml`).

slint::slint! {
    // Assets are addressed from here, not by a relative path repeated at every use. Slint resolves
    // an include path against each importing file's own folder, which is why `ui/` is flat and
    // sits as deep as this file does.
    #[include_path = "../../../assets"]

    export * from "../ui/app.slint";
}
