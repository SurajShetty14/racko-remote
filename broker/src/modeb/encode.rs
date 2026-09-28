//! Phase 2: push decoded RGBA frames into a GStreamer H.264 encode pipeline.
//!
//! Prefer `nvh264enc` (NVENC). Fall back to `x264enc` with a clear WARN when the
//! NVIDIA element is missing. Proof of encode is a finalized MP4 on disk; on a
//! box with an RTX GPU, `nvidia-smi` should show non-zero encoder utilization.

use core::pin::pin;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use ironrdp_session::image::DecodedImage;
use tracing::{info, warn};

use crate::config::{broker_dotenv_path, dotenv_map};
use crate::modeb::ModeBConfig;
use crate::modeb::connect;

/// Default Linux path for the Mode B encode proof MP4.
pub const ENCODED_MP4_PATH: &str = "/tmp/modeb-encoded.mp4";

pub(super) const DEFAULT_FPS: u32 = 30;
const DEFAULT_DURATION_SECS: u64 = 10;

/// Encode-session settings (output path, duration, fps).
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub output_path: PathBuf,
    pub duration: Duration,
    pub fps: u32,
}

impl EncodeConfig {
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        let output_path = lookup("MODEB_ENCODE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(ENCODED_MP4_PATH));

        let duration_secs: u64 = lookup("MODEB_ENCODE_DURATION_SECS")
            .unwrap_or_else(|| DEFAULT_DURATION_SECS.to_string())
            .parse()
            .context("MODEB_ENCODE_DURATION_SECS must be a whole number of seconds")?;
        if duration_secs == 0 {
            bail!("MODEB_ENCODE_DURATION_SECS must be >= 1");
        }

        let fps: u32 = lookup("MODEB_ENCODE_FPS")
            .unwrap_or_else(|| DEFAULT_FPS.to_string())
            .parse()
            .context("MODEB_ENCODE_FPS must be a positive integer")?;
        if fps == 0 {
            bail!("MODEB_ENCODE_FPS must be >= 1");
        }

        Ok(Self {
            output_path,
            duration: Duration::from_secs(duration_secs),
            fps,
        })
    }
}

/// Connect, decode for `encode.duration`, push RGBA into H.264, finalize MP4.
pub async fn run_encode(rdp: ModeBConfig, encode: EncodeConfig) -> anyhow::Result<()> {
    info!(
        output = %encode.output_path.display(),
        duration_secs = encode.duration.as_secs(),
        fps = encode.fps,
        "Mode B encode probe starting"
    );

    gst::init().context("gstreamer init")?;

    let output_path = &encode.output_path;
    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir for {}", output_path.display()))?;
        }
    }
    let location = output_path
        .to_str()
        .context("output path is not UTF-8")?
        .replace('\\', "/");

    let (connection_result, framed) = connect::connect(&rdp).await.context("Mode B connect")?;
    let width = connection_result.desktop_size.width;
    let height = connection_result.desktop_size.height;

    info!(
        width,
        height,
        pixel_format = "RGBA",
        compression = ?connection_result.compression_type,
        "Negotiated session parameters"
    );

    let tail = format!(
        "h264parse name=modeb-parse \
         ! mp4mux name=modeb-mux \
         ! filesink name=modeb-sink location=\"{location}\" sync=false"
    );
    let mut encoder =
        H264Pipeline::build(width, height, encode.fps, &tail, false).context("build H.264 encode pipeline")?;

    info!(
        encoder = encoder.encoder_name(),
        hardware = encoder.is_hardware(),
        "Selected H.264 encoder element"
    );

    encoder.play()?;

    let mut image = DecodedImage::new(ironrdp_graphics::image_processing::PixelFormat::RgbA32, width, height);

    let duration = encode.duration;
    let stop = pin!(async move {
        tokio::time::sleep(duration).await;
        Ok::<_, anyhow::Error>("encode duration elapsed")
    });
    connect::active_session_encode(connection_result, framed, &mut image, encode.fps, stop, &mut encoder)
        .await
        .context("Mode B encode session")?;

    info!(
        frames_pushed = encoder.frames_pushed(),
        path = %output_path.display(),
        "Sending EOS to finalize MP4"
    );
    encoder.finish().context("finalize MP4")?;
    info!(path = %output_path.display(), "Mode B encode probe complete");
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderKind {
    NvH264,
    X264,
}

impl EncoderKind {
    fn element_name(self) -> &'static str {
        match self {
            Self::NvH264 => "nvh264enc",
            Self::X264 => "x264enc",
        }
    }

    fn is_hardware(self) -> bool {
        matches!(self, Self::NvH264)
    }

    /// Element fragment used inside `gst::parse::launch`; one keyframe per second.
    fn launch_fragment(self, fps: u32) -> String {
        match self {
            // bitrate is kbit/sec on both encoders.
            Self::NvH264 => format!("nvh264enc preset=low-latency-hq bitrate=4000 gop-size={fps}"),
            Self::X264 => {
                format!("x264enc tune=zerolatency speed-preset=ultrafast bitrate=4000 key-int-max={fps}")
            }
        }
    }
}

fn select_encoder() -> anyhow::Result<EncoderKind> {
    if gst::ElementFactory::find("nvh264enc").is_some() {
        info!(element = "nvh264enc", "Using NVIDIA NVENC (nvh264enc)");
        Ok(EncoderKind::NvH264)
    } else if gst::ElementFactory::find("x264enc").is_some() {
        warn!(
            element = "x264enc",
            "nvh264enc not available; falling back to CPU software encoding (x264enc). \
             Install gst-plugins-bad with nvcodec and NVIDIA drivers for Mode B NVENC"
        );
        Ok(EncoderKind::X264)
    } else {
        bail!("neither nvh264enc nor x264enc GStreamer elements are available")
    }
}

/// appsrc(RGBA) → [leaky queue] → videoconvert → {nvh264enc|x264enc} → `tail`
///
/// The pipeline is set to NULL on drop.
pub(super) struct H264Pipeline {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    video_info: gst_video::VideoInfo,
    kind: EncoderKind,
    fps: u32,
    live: bool,
    frame_index: u64,
}

impl H264Pipeline {
    /// Build (but do not start) the pipeline; `tail` is linked after the encoder.
    ///
    /// A `live` pipeline stamps buffers with the pipeline clock and drops frames
    /// instead of blocking the RDP decode loop when downstream stalls.
    /// Otherwise buffers are stamped from the frame index and `push_frame` blocks.
    pub(super) fn build(width: u16, height: u16, fps: u32, tail: &str, live: bool) -> anyhow::Result<Self> {
        let kind = select_encoder()?;
        let width_u32 = u32::from(width);
        let height_u32 = u32::from(height);

        let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, width_u32, height_u32)
            .fps(gst::Fraction::new(i32::try_from(fps).context("fps fits i32")?, 1))
            .build()
            .context("build VideoInfo")?;

        let (do_timestamp, queue) = if live {
            (
                "true",
                "! queue name=modeb-queue leaky=downstream max-size-buffers=2 max-size-bytes=0 max-size-time=0 ",
            )
        } else {
            ("false", "")
        };

        // Caps are also set on appsrc below so push_buffer validates format/size.
        let launch = format!(
            "appsrc name=modeb-src is-live=true format=time do-timestamp={do_timestamp} \
             caps=video/x-raw,format=RGBA,width={width_u32},height={height_u32},framerate={fps}/1 \
             {queue}! videoconvert name=modeb-convert \
             ! {encoder} name=modeb-enc \
             ! {tail}",
            encoder = kind.launch_fragment(fps),
        );

        info!(pipeline = %launch, "Building GStreamer pipeline");

        let pipeline = gst::parse::launch(&launch)
            .context("parse GStreamer pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("parsed launch is not a Pipeline"))?;

        let appsrc = pipeline
            .by_name("modeb-src")
            .context("pipeline missing modeb-src")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("modeb-src is not an AppSrc"))?;

        appsrc.set_format(gst::Format::Time);
        appsrc.set_is_live(true);
        appsrc.set_block(!live);
        appsrc.set_max_bytes(u64::from(width_u32) * u64::from(height_u32) * 4 * 4);
        appsrc.set_caps(Some(&video_info.to_caps().context("video caps")?));

        Ok(Self {
            pipeline,
            appsrc,
            video_info,
            kind,
            fps,
            live,
            frame_index: 0,
        })
    }

    pub(super) fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    pub(super) fn play(&self) -> anyhow::Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("set pipeline Playing")?;
        Ok(())
    }

    pub(super) fn encoder_name(&self) -> &'static str {
        self.kind.element_name()
    }

    pub(super) fn is_hardware(&self) -> bool {
        self.kind.is_hardware()
    }

    pub(super) fn frames_pushed(&self) -> u64 {
        self.frame_index
    }

    fn push_rgba(&mut self, image: &DecodedImage) -> anyhow::Result<()> {
        let width = usize::from(image.width());
        let height = usize::from(image.height());
        let src_stride = width.checked_mul(4).context("source stride overflow")?;
        let pixels = image.data();
        let needed = src_stride
            .checked_mul(height)
            .context("framebuffer byte length overflow")?;
        if pixels.len() < needed {
            bail!(
                "framebuffer too small: have {} bytes, need {} ({}x{})",
                pixels.len(),
                needed,
                width,
                height
            );
        }

        let mut buffer = gst::Buffer::with_size(self.video_info.size()).context("allocate gst buffer")?;
        {
            let buffer = buffer.get_mut().context("gst buffer is not writable")?;
            let frame_ns = 1_000_000_000 / u64::from(self.fps);
            if !self.live {
                let pts =
                    gst::ClockTime::from_nseconds(self.frame_index.checked_mul(frame_ns).context("pts overflow")?);
                buffer.set_pts(pts);
            }
            buffer.set_duration(gst::ClockTime::from_nseconds(frame_ns));

            let mut vframe = gst_video::VideoFrameRef::from_buffer_ref_writable(buffer, &self.video_info)
                .context("wrap gst buffer as video frame")?;
            let dst_stride = usize::try_from(vframe.plane_stride()[0]).context("dst stride")?;
            let plane = vframe.plane_data_mut(0).context("plane 0")?;
            for row in 0..height {
                let src = &pixels[row * src_stride..(row + 1) * src_stride];
                let dst = &mut plane[row * dst_stride..row * dst_stride + src_stride];
                dst.copy_from_slice(src);
            }
        }

        match self.appsrc.push_buffer(buffer) {
            Ok(_) => {}
            Err(gst::FlowError::Flushing) => bail!("appsrc flushing while pushing frame"),
            Err(err) => bail!("appsrc push_buffer failed: {err}"),
        }

        self.frame_index = self.frame_index.saturating_add(1);
        if self.frame_index == 1 || self.frame_index.is_multiple_of(u64::from(self.fps) * 2) {
            info!(
                frames_pushed = self.frame_index,
                encoder = self.encoder_name(),
                "Pushed RGBA frames into encoder"
            );
        }

        drain_bus(&self.pipeline)?;
        Ok(())
    }

    /// Send EOS and wait for it to reach the sink (finalizes muxers such as mp4mux).
    fn finish(self) -> anyhow::Result<()> {
        self.appsrc.end_of_stream().context("appsrc end_of_stream")?;

        let bus = self.pipeline.bus().context("pipeline has no bus")?;
        let timeout = gst::ClockTime::from_seconds(30);
        match bus.timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error]) {
            Some(msg) => match msg.view() {
                gst::MessageView::Eos(..) => {
                    info!("Received EOS; pipeline finalized");
                }
                gst::MessageView::Error(err) => {
                    bail!(
                        "gstreamer error while finalizing: {} ({})",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                }
                _ => {}
            },
            None => bail!("timed out waiting for EOS while finalizing"),
        }

        self.pipeline.set_state(gst::State::Null).context("set pipeline Null")?;
        Ok(())
    }
}

impl Drop for H264Pipeline {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Surface errors, log warnings, and apply latency updates; discards other bus messages.
fn drain_bus(pipeline: &gst::Pipeline) -> anyhow::Result<()> {
    let Some(bus) = pipeline.bus() else {
        return Ok(());
    };
    while let Some(msg) = bus.pop_filtered(&[
        gst::MessageType::Error,
        gst::MessageType::Warning,
        gst::MessageType::Latency,
    ]) {
        match msg.view() {
            gst::MessageView::Error(err) => {
                bail!("gstreamer error: {} ({})", err.error(), err.debug().unwrap_or_default());
            }
            gst::MessageView::Warning(warn_msg) => {
                warn!(
                    error = %warn_msg.error(),
                    debug = %warn_msg.debug().unwrap_or_default(),
                    "gstreamer warning"
                );
            }
            gst::MessageView::Latency(..) => {
                if let Err(err) = pipeline.recalculate_latency() {
                    warn!(error = %err, "Failed to recalculate pipeline latency");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Called from the shared active-session encode loop.
pub(super) trait EncodeSink {
    fn push_frame(&mut self, image: &DecodedImage) -> anyhow::Result<()>;
}

impl EncodeSink for H264Pipeline {
    fn push_frame(&mut self, image: &DecodedImage) -> anyhow::Result<()> {
        self.push_rgba(image)
    }
}
