//! Mode B Phase 3a probe: stream the decoded RDP framebuffer to one browser over WebRTC.
//!
//! ```text
//! cargo run -p broker --features modeb-encode --bin modeb_webrtc_probe
//! ```
//!
//! Then open `http://127.0.0.1:8080/modeb.html` in a browser on the same box.
//! The RDP connection starts when the page opens the signaling WebSocket and
//! the probe exits when the browser disconnects or the RDP session ends.

#![allow(unused_crate_dependencies)] // thin binary; deps are exercised via the `broker` library

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new("info").expect("static info filter is valid"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let rdp = broker::modeb::ModeBConfig::load()?;
    let webrtc = broker::modeb::WebRtcConfig::load()?;
    broker::modeb::run_webrtc(rdp, webrtc).await
}
