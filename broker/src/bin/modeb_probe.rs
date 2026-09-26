//! Mode B Phase 1 probe: native IronRDP client that decodes the remote framebuffer.
//!
//! Credentials stay in the broker (bastion path). Proof of decode: periodic PNG dumps.
//!
//! ```text
//! cargo run -p broker --bin modeb_probe
//! ```

#![allow(unused_crate_dependencies)] // thin binary; deps are exercised via the `broker` library

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new("info").expect("static info filter is valid"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = broker::modeb::ModeBConfig::load()?;
    broker::modeb::run(config).await
}
