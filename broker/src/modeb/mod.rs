//! Mode B: bastion-side native IronRDP client that decodes the remote framebuffer.
//!
//! - Phase 1 (`modeb_probe`): PNG proof dumps of the RGBA framebuffer.
//! - Phase 2 (`modeb_encode_probe`, feature `modeb-encode`): H.264 via GStreamer NVENC.
//! - Phase 3a (`modeb_webrtc_probe`, feature `modeb-encode`): live H.264 over WebRTC.

mod connect;
mod framebuffer;

#[cfg(feature = "modeb-encode")]
mod encode;
#[cfg(feature = "modeb-encode")]
mod webrtc;

pub use connect::{ModeBConfig, run};
pub use framebuffer::FRAME_PNG_PATH;

#[cfg(feature = "modeb-encode")]
pub use encode::{ENCODED_MP4_PATH, EncodeConfig, run_encode};
#[cfg(feature = "modeb-encode")]
pub use webrtc::{WebRtcConfig, run_webrtc};
