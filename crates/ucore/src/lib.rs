//! Breeze μCore: Breeze Core for small machines.
//!
//! `no_std` with `alloc`, and no I/O of its own: a platform adapter (the host
//! build, an ESP32, ...) hands it time, randomness, sockets and storage
//! through [`platform::Platform`], and calls [`Core::poll`] in a loop. One
//! thread, bounded memory, the same API as Breeze Core for every route kept.
//! DESIGN.md has the why.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod api;
pub mod auth;
pub mod clock;
mod server;
pub mod http;
pub mod platform;
pub mod programs;
pub mod proto;
pub mod store;
pub mod timers;
pub mod unit;

pub use crate::server::{banner, Core, Settings, MAX_CLIENTS};

/// μCore's own version (the crates' semver).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Breeze Core native release whose behaviour μCore follows: its API, its
/// store formats, and the release the differential test compares against.
pub const NATIVE: &str = "4.3.2";

/// The product version people see: `1.0.0u4.3.2`.
pub fn product_version() -> alloc::string::String {
    alloc::format!("{VERSION}u{NATIVE}")
}

/// The version `/api/version` reports: the API spoken, with μCore's version as
/// semver build metadata, so a parser sees 4.3.2 and a printer shows both.
pub fn wire_version() -> alloc::string::String {
    alloc::format!("{NATIVE}+u{VERSION}")
}
