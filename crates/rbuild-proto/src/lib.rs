//! Shared types and wire protocol for rbuild's client (`rbuild`) and daemon
//! (`rbuildd`). Nothing in here performs I/O against the network directly —
//! it defines the frames, messages, config, and content-addressing the two
//! sides agree on.

pub mod chunk;
pub mod config;
pub mod hash;
pub mod merge;
pub mod proto;
pub mod scan;
pub mod transport;

pub use hash::Hash;
pub use proto::{Message, Target, PROTOCOL_VERSION};

/// Crate version string baked into the handshake so mismatches are visible.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
