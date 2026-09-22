//! Android platform layer for uplink.

pub mod audio;
pub mod camera;
pub mod codec;
pub mod cpu;
mod error;
pub mod log;
pub mod platform;
pub mod preview;

pub use error::Error;
