//! Racko RDP broker library.
//!
//! Hosts the RDCleanPath WebSocket relay (`session`) and Mode B native decode
//! path (`modeb`). The `broker` and `modeb_probe` binaries are thin entrypoints.

// Used by the package binaries; kept so `unused_crate_dependencies` stays quiet on the lib.
use tracing_subscriber as _;

pub mod api;
pub mod config;
pub mod modeb;
pub mod registry;
pub mod session;
