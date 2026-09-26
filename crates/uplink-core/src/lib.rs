//! Platform-agnostic core: identity, contacts, iroh endpoint, call signalling.

pub mod audio;
pub mod calls;
pub mod card;
pub mod contacts;
pub mod crypto;
pub mod db;
pub mod elapsed;
mod error;
pub mod health;
pub mod identity;
pub mod logs;
pub mod media;
pub mod node;
mod pilot;
pub mod preset;
mod protocol;
pub mod qr;
pub mod quality;
pub mod rate;
pub mod reach;
pub mod relays;
pub mod runtime;
pub mod settings;
mod telemetry;

pub use error::Error;
pub use iroh::address_lookup::MemoryLookup;
pub use iroh::{EndpointId, RelayUrl, SecretKey};
