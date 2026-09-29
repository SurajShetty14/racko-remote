//! Mode B: bastion-side native IronRDP client that decodes the remote framebuffer.
//!
//! - Phase 1 (`modeb_probe`): PNG proof dumps of the RGBA framebuffer.
//! - Phase 2 (`modeb_encode_probe`, feature `modeb-encode`): H.264 via GStreamer NVENC.
//! - Phase 3a/4 (`modeb_webrtc_probe`, deprecated): live H.264 over WebRTC with browser input.
//! - Phase 5b ([`ModeBService`], feature `modeb-encode`): the main broker serves
//!   concurrent sessions at `/modeb/webrtc` with per-session targets.

mod connect;
mod credentials;
mod framebuffer;

#[cfg(feature = "modeb-encode")]
mod encode;
#[cfg(feature = "modeb-encode")]
mod input;
#[cfg(feature = "modeb-encode")]
mod service;
#[cfg(feature = "modeb-encode")]
mod webrtc;

pub use connect::{ModeBConfig, run};
pub use credentials::{CredentialResolver, EnvCredentials, RdpCredentials};
pub use framebuffer::FRAME_PNG_PATH;

#[cfg(feature = "modeb-encode")]
pub use encode::{ENCODED_MP4_PATH, EncodeConfig, EncoderSettings, RateControl, run_encode};
#[cfg(feature = "modeb-encode")]
pub use service::{ModeBService, ModeBServiceConfig, SIGNALING_PATH};
#[cfg(feature = "modeb-encode")]
pub use webrtc::{WebRtcConfig, run_webrtc};
