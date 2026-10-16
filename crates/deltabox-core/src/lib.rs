//! deltabox-lite core library.
//!
//! Agent-agnostic sandbox sessions with checkpoint / diff / commit / abort.
//! The library only knows about sessions and change sets; adapters (Pi
//! extension, wrappers) decide review granularity.

pub mod backend;
#[cfg(target_os = "linux")]
pub mod cbcow;
pub mod changeset;
#[cfg(not(windows))]
pub mod checkpoint;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod command_executor;
#[cfg(not(windows))]
pub mod coordinator;
#[cfg(windows)]
#[path = "windows/coordinator.rs"]
pub mod coordinator;
#[cfg(not(windows))]
pub mod durable;
pub mod error;
pub mod journal;
#[cfg(target_os = "macos")]
pub mod macos_cbcow;
#[cfg(target_os = "macos")]
mod macos_tree;
pub mod pathmap;
pub mod policy;
#[cfg(target_os = "linux")]
mod private_publish;
#[cfg(not(windows))]
pub(crate) mod locks;
#[cfg(not(windows))]
pub(crate) mod project_lease;
#[cfg(not(windows))]
pub(crate) mod publog;
pub mod protocol;
#[cfg(not(windows))]
pub(crate) mod test_hooks;
pub mod server;
#[cfg(not(windows))]
pub mod session;
#[cfg(not(windows))]
mod state_layout;
#[cfg(windows)]
#[path = "windows/layout.rs"]
mod state_layout;
#[cfg(not(windows))]
pub mod snapshot;
#[cfg(not(windows))]
pub mod trusted;
pub mod worker;
#[cfg(windows)]
pub mod windows;

pub use error::{Error, Result};
