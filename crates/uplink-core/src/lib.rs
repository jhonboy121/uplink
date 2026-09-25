//! Platform-agnostic core: identity, contacts, iroh endpoint, call signalling.

pub mod audio;
pub mod calls;
pub mod card;
pub mod contacts;
pub mod crypto;
pub mod db;
mod error;
pub mod health;
pub mod identity;
pub mod logs;
pub mod media;
pub mod node;
pub mod qr;
pub mod quality;
mod pilot;
pub mod relays;
mod protocol;
pub mod reach;
pub mod runtime;
pub mod settings;
mod telemetry;

pub use error::Error;
pub use iroh::{EndpointId, RelayUrl, SecretKey};
pub use iroh::address_lookup::MemoryLookup;
