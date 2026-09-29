//! Phase 2: push decoded RGBA frames into a GStreamer H.264 encode pipeline.
//!
//! Prefer `nvh264enc` (NVENC). Fall back to `x264enc` with a clear WARN when the
//! NVIDIA element is missing. Proof of encode is a finalized MP4 on disk; on a
//! box with an RTX GPU, `nvidia-smi` should show non-zero encoder utilization.

use core::pin::pin;
use core::sync::atomic::{AtomicU64, Ordering};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

const DEFAULT_DURATION_SECS: u64 = 10;
const DEFAULT_FPS: u32 = 30;
const MAX_FPS: u32 = 60;
const DEFAULT_BITRATE_KBPS: u32 = 20_000;
/// NVENC `aq-strength` range; 0 lets the driver pick.
const MAX_AQ_STRENGTH: u32 = 15;
/// Frames the live pipeline may buffer ahead of the encoder before dropping the oldest.
const LIVE_QUEUE_BUFFERS: u32 = 4;
/// Interval of the "Encoder stats" log line.
const STATS_INTERVAL: Duration = Duration::from_secs(1);

/// `nvh264enc` rate control (`rc-mode`); ignored by the `x264enc` fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    /// Constant bitrate: steady bandwidth and latency, best for live streaming.
    Cbr,
    /// Low-delay high-quality CBR.
    CbrLdHq,
    /// High-quality CBR.
    CbrHq,
    /// Variable bitrate averaging `bitrate`, peaking at 1.5x.
    Vbr,
    /// High-quality VBR.
    VbrHq,
}

impl RateControl {
    fn nick(self) -> &'static str {
        match self {
            Self::Cbr => "cbr",
            Self::CbrLdHq => "cbr-ld-hq",
            Self::CbrHq => "cbr-hq",
            Self::Vbr => "vbr",
            Self::VbrHq => "vbr-hq",
        }
    }

    fn is_vbr(self) -> bool {
        matches!(self, Self::Vbr | Self::VbrHq)
    }
}

impl core::str::FromStr for RateControl {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "cbr" => Ok(Self::Cbr),
            "cbr-ld-hq" => Ok(Self::CbrLdHq),
            "cbr-hq" => Ok(Self::CbrHq),
            "vbr" => Ok(Self::Vbr),
            "vbr-hq" => Ok(Self::VbrHq),
            other => bail!("expected one of cbr, cbr-ld-hq, cbr-hq, vbr, vbr-hq, got {other}"),
        }
    }
}

/// Where RGBA is converted to the encoder's YUV input (`MODEB_CONVERT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorConvert {
    /// GPU when `cudaupload`/`cudaconvert` exist and the encoder is NVENC, otherwise CPU.
    Auto,
    /// `videoconvert` on the CPU.
    Cpu,
    /// `cudaupload ! cudaconvert` to NV12 in CUDA memory.
    Gpu,
}

impl core::str::FromStr for ColorConvert {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "gpu" => Ok(Self::Gpu),
            other => bail!("expected one of auto, cpu, gpu, got {other}"),
        }
    }
}

/// H.264 encoder settings shared by the MP4 and WebRTC probes.
#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    /// Capture and encode rate; the framebuffer is pushed at this rate whether or not RDP updated it.
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub rate_control: RateControl,
    /// NVENC adaptive quantization; ignored by the `x264enc` fallback.
    pub spatial_aq: bool,
    pub temporal_aq: bool,
    /// 1 (gentle) to 15 (aggressive); 0 lets the driver pick.
    pub aq_strength: u32,
    pub convert: ColorConvert,
}

impl EncoderSettings {
    /// `MODEB_FPS` (1..=60, falls back to legacy `MODEB_ENCODE_FPS`), `MODEB_BITRATE_KBPS`,
    /// `MODEB_RC_MODE`, `MODEB_SPATIAL_AQ`, `MODEB_TEMPORAL_AQ`, `MODEB_AQ_STRENGTH`, `MODEB_CONVERT`.
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        let fps: u32 = lookup("MODEB_FPS")
            .or_else(|| lookup("MODEB_ENCODE_FPS"))
            .map_or(Ok(DEFAULT_FPS), |v| v.parse())
            .context("MODEB_FPS must be a positive integer")?;
        if !(1..=MAX_FPS).contains(&fps) {
            bail!("MODEB_FPS must be between 1 and {MAX_FPS}, got {fps}");
        }

        let bitrate_kbps: u32 = lookup("MODEB_BITRATE_KBPS")
            .map_or(Ok(DEFAULT_BITRATE_KBPS), |v| v.parse())
            .context("MODEB_BITRATE_KBPS must be a positive integer (kbit/s)")?;
        if bitrate_kbps == 0 {
            bail!("MODEB_BITRATE_KBPS must be >= 1");
        }

        let rate_control = lookup("MODEB_RC_MODE")
            .map_or(Ok(RateControl::Vbr), |v| v.parse())
            .context("invalid MODEB_RC_MODE")?;

        let flag = |key: &str, default: bool| -> anyhow::Result<bool> {
            match lookup(key).as_deref() {
                None => Ok(default),
                Some("true" | "1") => Ok(true),
                Some("false" | "0") => Ok(false),
                Some(other) => bail!("{key} must be true or false, got {other}"),
            }
        };
        let spatial_aq = flag("MODEB_SPATIAL_AQ", true)?;
        let temporal_aq = flag("MODEB_TEMPORAL_AQ", false)?;

        let aq_strength: u32 = lookup("MODEB_AQ_STRENGTH")
            .map_or(Ok(0), |v| v.parse())
            .context("MODEB_AQ_STRENGTH must be an integer")?;
        if MAX_AQ_STRENGTH < aq_strength {
            bail!("MODEB_AQ_STRENGTH must be between 0 and {MAX_AQ_STRENGTH}, got {aq_strength}");
        }

        let convert = lookup("MODEB_CONVERT")
            .map_or(Ok(ColorConvert::Auto), |v| v.parse())
            .context("invalid MODEB_CONVERT")?;

        Ok(Self {
            fps,
            bitrate_kbps,
            rate_control,
            spatial_aq,
            temporal_aq,
            aq_strength,
            convert,
        })
    }
}

/// Encode-session settings (output path, duration, encoder).
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub output_path: PathBuf,
    pub duration: Duration,
    pub encoder: EncoderSettings,
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

        Ok(Self {
            output_path,
            duration: Duration::from_secs(duration_secs),
            encoder: EncoderSettings::load()?,
        })
    }
}

/// Connect, decode for `encode.duration`, push RGBA into H.264, finalize MP4.
pub async fn run_encode(rdp: ModeBConfig, encode: EncodeConfig) -> anyhow::Result<()> {
    info!(
        output = %encode.output_path.display(),
        duration_secs = encode.duration.as_secs(),
        settings = ?encode.encoder,
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
        H264Pipeline::build(width, height, &encode.encoder, &tail, false).context("build H.264 encode pipeline")?;

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
    connect::active_session_encode(
        connection_result,
        framed,
        &mut image,
        encode.encoder.fps,
        stop,
        None,
        &mut encoder,
    )
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

    /// Element fragment used inside `gst::parse::launch`.
    fn launch_fragment(self, settings: &EncoderSettings) -> String {
        // bitrate and max-bitrate are kbit/sec on both encoders.
        let bitrate = settings.bitrate_kbps;
        // One keyframe per second at any fps.
        let gop = settings.fps;
        match self {
            Self::NvH264 => {
                let rc_mode = settings.rate_control;
                let max_bitrate = if rc_mode.is_vbr() {
                    format!(" max-bitrate={}", bitrate.saturating_mul(3) / 2)
                } else {
                    String::new()
                };
                format!(
                    "nvh264enc preset=low-latency-hq rc-mode={} bitrate={bitrate}{max_bitrate} gop-size={gop}",
                    rc_mode.nick()
                )
            }
            Self::X264 => {
                format!("x264enc tune=zerolatency speed-preset=ultrafast bitrate={bitrate} key-int-max={gop}")
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

/// Encoded output, counted by a probe on the encoder's src pad.
#[derive(Default)]
struct EncodedCounters {
    bytes: AtomicU64,
    frames: AtomicU64,
    keyframes: AtomicU64,
}

/// Counter values at the previous "Encoder stats" line.
struct StatsWindow {
    at: Instant,
    pushed: u64,
    encoded_bytes: u64,
    encoded_frames: u64,
    keyframes: u64,
    dropped: u64,
}

/// appsrc(RGBA) → [leaky queue] → {videoconvert|cudaupload → cudaconvert} → {nvh264enc|x264enc} → `tail`
///
/// The pipeline is set to NULL on drop.
pub(super) struct H264Pipeline {
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    video_info: gst_video::VideoInfo,
    kind: EncoderKind,
    settings: EncoderSettings,
    /// `"cpu"` or `"gpu"`: where RGBA is converted for the encoder.
    convert: &'static str,
    live: bool,
    frame_index: u64,
    /// Live pipelines only: the leaky queue and how many times it overflowed (one dropped frame each).
    queue: Option<(gst::Element, Arc<AtomicU64>)>,
    encoded: Arc<EncodedCounters>,
    window: StatsWindow,
    input_caps_logged: bool,
}

impl H264Pipeline {
    /// Build (but do not start) the pipeline; `tail` is linked after the encoder.
    ///
    /// A `live` pipeline stamps buffers with the pipeline clock and, when the
    /// encoder falls behind, drops the oldest queued frame instead of blocking
    /// the RDP decode loop. Otherwise buffers are stamped from the frame index
    /// and `push_frame` blocks.
    pub(super) fn build(
        width: u16,
        height: u16,
        settings: &EncoderSettings,
        tail: &str,
        live: bool,
    ) -> anyhow::Result<Self> {
        let kind = select_encoder()?;
        let fps = settings.fps;
        let width_u32 = u32::from(width);
        let height_u32 = u32::from(height);

        let video_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, width_u32, height_u32)
            .fps(gst::Fraction::new(i32::try_from(fps).context("fps fits i32")?, 1))
            .build()
            .context("build VideoInfo")?;

        // The queue is empty while the encoder keeps up, so it adds no latency
        // then; it only fills (up to LIVE_QUEUE_BUFFERS frames) during bursts.
        let (do_timestamp, queue) = if live {
            (
                "true",
                format!(
                    "! queue name=modeb-queue leaky=downstream max-size-buffers={LIVE_QUEUE_BUFFERS} \
                     max-size-bytes=0 max-size-time=0 "
                ),
            )
        } else {
            ("false", String::new())
        };

        let has_cuda_convert = ["cudaupload", "cudaconvert"]
            .iter()
            .all(|name| gst::ElementFactory::find(name).is_some());
        let gpu_convert = match settings.convert {
            ColorConvert::Cpu => false,
            ColorConvert::Auto => kind.is_hardware() && has_cuda_convert,
            ColorConvert::Gpu if kind.is_hardware() && has_cuda_convert => true,
            ColorConvert::Gpu => {
                warn!(
                    encoder = kind.element_name(),
                    has_cuda_convert,
                    "MODEB_CONVERT=gpu needs nvh264enc, cudaupload and cudaconvert; using videoconvert"
                );
                false
            }
        };
        let convert_label = if gpu_convert { "gpu" } else { "cpu" };
        let convert = if gpu_convert {
            "cudaupload name=modeb-upload ! cudaconvert name=modeb-convert \
             ! video/x-raw(memory:CUDAMemory),format=NV12"
        } else {
            "videoconvert name=modeb-convert"
        };

        // Caps are also set on appsrc below so push_buffer validates format/size.
        let launch = format!(
            "appsrc name=modeb-src is-live=true format=time do-timestamp={do_timestamp} \
             caps=video/x-raw,format=RGBA,width={width_u32},height={height_u32},framerate={fps}/1 \
             {queue}! {convert} \
             ! {encoder} name=modeb-enc \
             ! {tail}",
            encoder = kind.launch_fragment(settings),
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

        let queue = match pipeline.by_name("modeb-queue") {
            Some(queue) => {
                let overruns = Arc::new(AtomicU64::new(0));
                let counter = Arc::clone(&overruns);
                queue.connect("overrun", false, move |_| {
                    counter.fetch_add(1, Ordering::Relaxed);
                    None
                });
                Some((queue, overruns))
            }
            None => None,
        };

        let enc = pipeline.by_name("modeb-enc").context("pipeline missing modeb-enc")?;
        if kind == EncoderKind::NvH264 {
            // Set here rather than in the launch string: older nvcodec builds lack some of these,
            // and an unknown property there fails the whole pipeline.
            for (name, value) in [
                ("spatial-aq", settings.spatial_aq.to_value()),
                ("temporal-aq", settings.temporal_aq.to_value()),
                ("aq-strength", settings.aq_strength.to_value()),
            ] {
                match enc.find_property(name) {
                    Some(pspec) if pspec.value_type() == value.type_() => enc.set_property_from_value(name, &value),
                    _ => warn!(property = name, "nvh264enc has no such property; skipping"),
                }
            }
            // Read back from the element, so the log shows what NVENC actually uses.
            let read = |name: &str| enc.find_property(name).map(|_| enc.property_value(name));
            let read_bool = |name: &str| read(name).and_then(|value| value.get::<bool>().ok());
            let read_u32 = |name: &str| read(name).and_then(|value| value.get::<u32>().ok());
            info!(
                rc_mode = settings.rate_control.nick(),
                bitrate_kbps = settings.bitrate_kbps,
                spatial_aq = ?read_bool("spatial-aq"),
                temporal_aq = ?read_bool("temporal-aq"),
                aq_strength = ?read_u32("aq-strength"),
                bframes = ?read_u32("bframes"),
                rc_lookahead = ?read_u32("rc-lookahead"),
                gop = fps,
                convert = convert_label,
                "Configured NVENC"
            );
        }

        let encoded = Arc::new(EncodedCounters::default());
        {
            let encoded = Arc::clone(&encoded);
            enc.static_pad("src").context("modeb-enc has no src pad")?.add_probe(
                gst::PadProbeType::BUFFER,
                move |_pad, info| {
                    if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                        encoded
                            .bytes
                            .fetch_add(u64::try_from(buffer.size()).unwrap_or(u64::MAX), Ordering::Relaxed);
                        encoded.frames.fetch_add(1, Ordering::Relaxed);
                        if !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT) {
                            encoded.keyframes.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }

        Ok(Self {
            pipeline,
            appsrc,
            video_info,
            kind,
            settings: *settings,
            convert: convert_label,
            live,
            frame_index: 0,
            queue,
            encoded,
            window: StatsWindow {
                at: Instant::now(),
                pushed: 0,
                encoded_bytes: 0,
                encoded_frames: 0,
                keyframes: 0,
                dropped: 0,
            },
            input_caps_logged: false,
        })
    }

    pub(super) fn pipeline(&self) -> &gst::Pipeline {
        &self.pipeline
    }

    pub(super) fn play(&self) -> anyhow::Result<()> {
        self.set_state(gst::State::Playing)
    }

    /// Change state, reporting the bus error behind a refusal (such as NVENC failing to open).
    pub(super) fn set_state(&self, state: gst::State) -> anyhow::Result<()> {
        if self.pipeline.set_state(state).is_err() {
            drain_bus(&self.pipeline)?;
            bail!("pipeline refused state {state:?}");
        }
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
            let frame_ns = 1_000_000_000 / u64::from(self.settings.fps);
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

        let now = Instant::now();
        let elapsed = now.duration_since(self.window.at);
        if STATS_INTERVAL <= elapsed {
            if !self.input_caps_logged {
                // RGBA here means the converter passed frames through and NVENC converts them itself.
                let caps = self
                    .pipeline
                    .by_name("modeb-enc")
                    .and_then(|enc| enc.static_pad("sink"))
                    .and_then(|pad| pad.current_caps());
                if let Some(caps) = caps {
                    info!(convert = self.convert, %caps, "Encoder input negotiated");
                    self.input_caps_logged = true;
                }
            }

            let secs = elapsed.as_secs_f64();
            let per_sec = |delta: u64| format!("{:.1}", delta as f64 / secs);
            let encoded_bytes = self.encoded.bytes.load(Ordering::Relaxed);
            let encoded_frames = self.encoded.frames.load(Ordering::Relaxed);
            let keyframes = self.encoded.keyframes.load(Ordering::Relaxed);
            let dropped = self
                .queue
                .as_ref()
                .map_or(0, |(_, overruns)| overruns.load(Ordering::Relaxed));
            let queue_level = self
                .queue
                .as_ref()
                .map(|(queue, _)| queue.property::<u32>("current-level-buffers"));
            let encoded_kbps = (encoded_bytes - self.window.encoded_bytes) as f64 * 8.0 / 1000.0 / secs;
            info!(
                encoder = self.encoder_name(),
                convert = self.convert,
                rc_mode = self.settings.rate_control.nick(),
                target_kbps = self.settings.bitrate_kbps,
                encoded_kbps = format!("{encoded_kbps:.0}"),
                pushed_fps = per_sec(self.frame_index - self.window.pushed),
                encoded_fps = per_sec(encoded_frames - self.window.encoded_frames),
                keyframes = keyframes - self.window.keyframes,
                queue_level = ?queue_level,
                queue_max = LIVE_QUEUE_BUFFERS,
                dropped = dropped - self.window.dropped,
                dropped_total = dropped,
                "Encoder stats"
            );
            self.window = StatsWindow {
                at: now,
                pushed: self.frame_index,
                encoded_bytes,
                encoded_frames,
                keyframes,
                dropped,
            };
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
                let source = err.src().map(|src| src.name().to_string()).unwrap_or_default();
                let is_nvenc = source == "modeb-enc"
                    && err
                        .src()
                        .and_then(|src| src.downcast_ref::<gst::Element>())
                        .and_then(|element| element.factory())
                        .is_some_and(|factory| factory.name() == EncoderKind::NvH264.element_name());
                if is_nvenc {
                    bail!(
                        "nvh264enc failed: {} ({}); the GPU may be at its concurrent NVENC session limit",
                        err.error(),
                        err.debug().unwrap_or_default()
                    );
                }
                bail!(
                    "gstreamer error from {source}: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
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
