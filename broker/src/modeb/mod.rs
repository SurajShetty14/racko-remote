//! Mode B: bastion-side native IronRDP client that decodes the remote framebuffer.
//!
//! - Phase 1 (`modeb_probe`): PNG proof dumps of the RGBA framebuffer.
//! - Phase 2 (`modeb_encode_probe`, feature `modeb-encode`): H.264 via GStreamer NVENC.

mod connect;
mod framebuffer;

#[cfg(feature = "modeb-encode")]
mod encode;

pub use connect::{ModeBConfig, run};
pub use framebuffer::FRAME_PNG_PATH;

#[cfg(feature = "modeb-encode")]
pub use encode::{EncodeConfig, ENCODED_MP4_PATH, run_encode};
