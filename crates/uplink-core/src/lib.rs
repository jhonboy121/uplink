//! Platform-agnostic core: identity, contacts, iroh endpoint, call signalling.

pub mod audio;
pub mod calls;
pub mod contacts;
pub mod crypto;
mod error;
pub mod identity;
pub mod media;
pub mod node;
pub mod qr;
mod protocol;
pub mod runtime;
mod telemetry;

pub use error::Error;
pub use iroh::EndpointId;
pub use iroh::address_lookup::MemoryLookup;
