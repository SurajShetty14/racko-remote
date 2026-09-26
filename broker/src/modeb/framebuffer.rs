//! In-memory RGBA framebuffer helpers and PNG proof dumps.

use std::path::Path;

use anyhow::Context as _;
use ironrdp_session::image::DecodedImage;

/// Default Linux path for the Mode B decode proof PNG.
pub const FRAME_PNG_PATH: &str = "/tmp/modeb-frame.png";

/// Write the current decoded desktop as an RGBA PNG.
pub(super) fn write_png(image: &DecodedImage, path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", path.display()))?;
        }
    }

    let width = u32::from(image.width());
    let height = u32::from(image.height());
    let buffer: image::ImageBuffer<image::Rgba<u8>, _> =
        image::ImageBuffer::from_raw(width, height, image.data().to_vec())
            .context("DecodedImage dimensions do not match RGBA byte length")?;

    buffer
        .save(path)
        .with_context(|| format!("save PNG to {}", path.display()))?;

    Ok(())
}
