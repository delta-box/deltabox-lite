//! deltabox-lite core library.
//!
//! Agent-agnostic sandbox sessions with checkpoint / diff / commit / abort.
//! The library only knows about sessions and change sets; adapters (Pi
//! extension, wrappers) decide review granularity.

pub mod backend;
pub mod changeset;
pub mod checkpoint;
pub mod durable;
pub mod error;
pub mod journal;
pub mod policy;
pub mod protocol;
pub mod server;
pub mod session;
pub mod snapshot;
pub mod worker;

pub use error::{Error, Result};
