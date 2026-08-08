//! deltabox-lite core library.
//!
//! Agent-agnostic sandbox sessions with checkpoint / diff / commit / abort.
//! The library only knows about sessions and change sets; adapters (Pi
//! extension, wrappers) decide review granularity.

pub mod backend;
#[cfg(target_os = "linux")]
pub mod cbcow;
pub mod changeset;
pub mod checkpoint;
pub mod coordinator;
pub mod durable;
pub mod error;
pub mod journal;
pub mod pathmap;
pub mod policy;
pub mod protocol;
pub mod server;
pub mod session;
pub mod snapshot;
pub mod worker;

pub use error::{Error, Result};
