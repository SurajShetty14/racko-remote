//! Mode B Phase 2 probe: decode RDP framebuffer and GPU-encode H.264 to MP4.
//!
//! Prefer NVENC (`nvh264enc`). Falls back to `x264enc` with a WARN when NVENC is missing.
//!
//! ```text
//! cargo run -p broker --features modeb-encode --bin modeb_encode_probe
//! ```
//!
//! While running on a GPU host, check encoder load:
//! `nvidia-smi dmon -s u` (Enc column should be non-zero with nvh264enc).

#![allow(unused_crate_dependencies)] // thin binary; deps are exercised via the `broker` library

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new("info").expect("static info filter is valid"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let rdp = broker::modeb::ModeBConfig::load()?;
    let encode = broker::modeb::EncodeConfig::load()?;
    broker::modeb::run_encode(rdp, encode).await
}
