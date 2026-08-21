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
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod command_executor;
pub mod coordinator;
pub mod durable;
pub mod error;
#[cfg(target_os = "linux")]
mod file_lock;
#[cfg(target_os = "linux")]
pub mod git_session;
pub mod journal;
#[cfg(target_os = "macos")]
pub mod macos_cbcow;
#[cfg(target_os = "macos")]
mod macos_tree;
pub mod pathmap;
pub mod policy;
#[cfg(target_os = "linux")]
mod private_publish;
pub mod protocol;
pub mod providers;
pub mod server;
pub mod session;
pub mod snapshot;
mod state_layout;
pub mod worker;

pub use error::{Error, Result};
