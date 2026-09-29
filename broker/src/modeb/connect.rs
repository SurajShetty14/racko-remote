//! Async TCP+TLS connect and active-session loop (native IronRDP, not WASM).

use core::time::Duration;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use ironrdp_connector::{ClientConnector, ConnectionResult, Credentials, DesktopSize};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::gcc::{ConnectionType, KeyboardType};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{CompressionType, PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tls::CertificateValidation;
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::{FramedWrite as _, TokioFramed};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::time::{Instant, interval};
use tracing::{debug, info, trace, warn};

use crate::config::{broker_dotenv_path, dotenv_map, split_host_port};
use crate::modeb::framebuffer::{self, FRAME_PNG_PATH};

pub(super) trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncReadWrite for T {}

pub(super) type UpgradedFramed = TokioFramed<Box<dyn AsyncReadWrite + Unpin + Send + Sync>>;

/// RDP target and the credentials the broker signs in with (never echoed back to the browser).
#[derive(Clone)]
pub struct ModeBConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub tls_insecure: bool,
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub frame_path: PathBuf,
    pub dump_interval: Duration,
}

impl ModeBConfig {
    /// Load from process env and `broker/.env` (same lookup rules as the relay).
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());

        let target =
            lookup(&file, "RDP_TARGET").context("RDP_TARGET is required (host:port). Set it in broker/.env")?;
        let (host, port) = split_host_port(&target)?;

        let username = lookup(&file, "RDP_USERNAME").context("RDP_USERNAME is required for Mode B")?;
        let password = lookup(&file, "RDP_PASSWORD").context("RDP_PASSWORD is required for Mode B")?;
        let domain = lookup(&file, "RDP_DOMAIN").filter(|d| !d.is_empty());

        let tls_mode = lookup(&file, "RDP_TLS_VERIFY").unwrap_or_else(|| "insecure".to_owned());
        let tls_insecure = match tls_mode.as_str() {
            "insecure" => true,
            "strict" => false,
            other => bail!("RDP_TLS_VERIFY must be \"insecure\" or \"strict\", got {other}"),
        };

        let desktop_width = lookup(&file, "MODEB_DESKTOP_WIDTH")
            .unwrap_or_else(|| "1280".to_owned())
            .parse()
            .context("MODEB_DESKTOP_WIDTH must be a u16")?;
        let desktop_height = lookup(&file, "MODEB_DESKTOP_HEIGHT")
            .unwrap_or_else(|| "1024".to_owned())
            .parse()
            .context("MODEB_DESKTOP_HEIGHT must be a u16")?;

        let frame_path = lookup(&file, "MODEB_FRAME_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(FRAME_PNG_PATH));

        let dump_secs: u64 = lookup(&file, "MODEB_DUMP_INTERVAL_SECS")
            .unwrap_or_else(|| "2".to_owned())
            .parse()
            .context("MODEB_DUMP_INTERVAL_SECS must be a whole number of seconds")?;
        if dump_secs == 0 {
            bail!("MODEB_DUMP_INTERVAL_SECS must be >= 1");
        }

        Ok(Self {
            host,
            port,
            username,
            password,
            domain,
            tls_insecure,
            desktop_width,
            desktop_height,
            frame_path,
            dump_interval: Duration::from_secs(dump_secs),
        })
    }

    /// Settings for one broker-hosted session to an already allow-listed target.
    pub fn for_target(
        host: String,
        port: u16,
        credentials: super::RdpCredentials,
        tls_insecure: bool,
        desktop_width: u16,
        desktop_height: u16,
    ) -> Self {
        Self {
            host,
            port,
            username: credentials.username,
            password: credentials.password,
            domain: credentials.domain,
            tls_insecure,
            desktop_width,
            desktop_height,
            frame_path: PathBuf::from(FRAME_PNG_PATH),
            dump_interval: Duration::from_secs(2),
        }
    }

    pub(super) fn destination_label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl core::fmt::Debug for ModeBConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ModeBConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .field("tls_insecure", &self.tls_insecure)
            .field("desktop_width", &self.desktop_width)
            .field("desktop_height", &self.desktop_height)
            .finish_non_exhaustive()
    }
}

/// Fit a browser-requested desktop size inside 1920 × 1200 (bounding NVENC load and bitrate),
/// keeping its aspect ratio, then round each side down to an even number.
///
/// Even sides are required by the H.264 4:2:0 encode; 200 is the smallest desktop side
/// servers accept ([MS-RDPBCGR 2.2.1.3.2]).
///
/// [MS-RDPBCGR 2.2.1.3.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/00f1da4a-ee9c-421a-852f-c19f92343d73
pub fn fit_desktop_size(width: u32, height: u32) -> (u16, u16) {
    const MIN_SIDE: u64 = 200;
    let max_width: u64 = 1920;
    let max_height: u64 = 1200;

    let (mut width, mut height) = (u64::from(width.max(1)), u64::from(height.max(1)));
    if max_width < width {
        height = height * max_width / width;
        width = max_width;
    }
    if max_height < height {
        width = width * max_height / height;
        height = max_height;
    }

    let side = |value: u64, max: u64| u16::try_from(value.clamp(MIN_SIDE, max) & !1).unwrap_or(u16::MAX);
    (side(width, max_width), side(height, max_height))
}

fn lookup(file: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    std::env::var(key).ok().or_else(|| file.get(key).cloned())
}

/// Connect, decode framebuffer updates, and dump PNG every `dump_interval`.
pub async fn run(config: ModeBConfig) -> anyhow::Result<()> {
    info!(
        target = %config.destination_label(),
        username = %config.username,
        domain = ?config.domain,
        tls_insecure = config.tls_insecure,
        frame_path = %config.frame_path.display(),
        dump_interval_secs = config.dump_interval.as_secs(),
        "Mode B probe starting (server-side auth)"
    );

    let (connection_result, framed) = connect(&config).await.context("Mode B connect")?;

    let pixel_format = PixelFormat::RgbA32;
    info!(
        width = connection_result.desktop_size.width,
        height = connection_result.desktop_size.height,
        ?pixel_format,
        compression = ?connection_result.compression_type,
        enable_server_pointer = connection_result.enable_server_pointer,
        "Negotiated session parameters"
    );

    let mut image = DecodedImage::new(
        pixel_format,
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );

    active_session_png(connection_result, framed, &mut image, &config)
        .await
        .context("Mode B active session")?;

    Ok(())
}

pub(super) async fn connect(config: &ModeBConfig) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let dest = config.destination_label();
    let stream = TcpStream::connect(&dest)
        .await
        .with_context(|| format!("TCP connect to {dest}"))?;
    let client_addr = stream.local_addr().context("get socket local address")?;

    let mut framed = TokioFramed::new(stream);
    let mut connector = ClientConnector::new(build_connector_config(config)?, client_addr);

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .context("connect begin")?;

    debug!("TLS upgrade");

    let (initial_stream, leftover_bytes) = framed.into_inner();
    let validation = if config.tls_insecure {
        CertificateValidation::DangerouslyAcceptInvalidCertificate
    } else {
        CertificateValidation::Strict
    };
    let (tls_stream, tls_cert) =
        ironrdp_tls::upgrade_with_certificate_validation(initial_stream, &config.host, validation)
            .await
            .context("TLS upgrade")?;

    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let erased: Box<dyn AsyncReadWrite + Unpin + Send + Sync> = Box::new(tls_stream);
    let mut upgraded_framed = TokioFramed::new_with_leftover(erased, leftover_bytes);

    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_cert)
        .context("unable to extract TLS server public key")?
        .to_owned();

    let connection_result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut ReqwestNetworkClient::new(),
        config.host.clone().into(),
        server_public_key,
        None,
    )
    .await
    .context("connect finalize")?;

    Ok((connection_result, upgraded_framed))
}

/// Whether [`connect`] failed because the target rejected the credentials during CredSSP (NLA),
/// rather than a network, TLS, or protocol failure.
#[cfg(feature = "modeb-encode")]
pub(super) fn is_auth_failure(err: &anyhow::Error) -> bool {
    use ironrdp_connector::{ConnectorError, ConnectorErrorKind, sspi};

    err.chain().any(|cause| {
        cause
            .downcast_ref::<ConnectorError>()
            .is_some_and(|error| match error.kind() {
                ConnectorErrorKind::AccessDenied => true,
                // The server's TSRequest errorCode (STATUS_LOGON_FAILURE, STATUS_WRONG_PASSWORD, ...).
                ConnectorErrorKind::Credssp(error) => {
                    error.nstatus.is_some()
                        || matches!(
                            error.error_type,
                            sspi::ErrorKind::LogonDenied | sspi::ErrorKind::NoCredentials
                        )
                }
                _ => false,
            })
    })
}

fn build_connector_config(config: &ModeBConfig) -> anyhow::Result<ironrdp_connector::Config> {
    Ok(ironrdp_connector::Config {
        credentials: Credentials::UsernamePassword {
            username: config.username.clone(),
            password: config.password.clone(),
        },
        domain: config.domain.clone(),
        // Prefer NLA (CredSSP); disable legacy graphical-login TLS.
        enable_tls: false,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        connection_type: ConnectionType::Lan,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: config.desktop_width,
            height: config.desktop_height,
        },
        monitor_layout: None,
        bitmap: None,
        client_build: 0,
        client_name: "racko-modeb".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: Some(CompressionType::Rdp61),
        pointer_software_rendering: true,
        multitransport_flags: None,
        // Bitmap graphics only for Phase 1 (no EGFX handler registered).
        support_dyn_vc_gfx_protocol: false,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp_pdu::rdp::capability_sets::RailSupportLevel::empty(),
    })
}

async fn active_session_png(
    connection_result: ConnectionResult,
    framed: UpgradedFramed,
    image: &mut DecodedImage,
    config: &ModeBConfig,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = ironrdp_tokio::split_tokio_framed(framed);

    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    let mut update_count: u64 = 0;
    let mut frame_count: u64 = 0;
    let mut dump_ticks: u64 = 0;
    let started = Instant::now();
    let mut dump = interval(config.dump_interval);
    dump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    dump.tick().await;

    info!(
        path = %config.frame_path.display(),
        interval_secs = config.dump_interval.as_secs(),
        "Active session started; dumping framebuffer periodically"
    );

    loop {
        tokio::select! {
            frame = reader.read_pdu() => {
                let (action, payload) = frame.context("read PDU")?;
                frame_count = frame_count.saturating_add(1);
                trace!(?action, frame_length = payload.len(), frame_count, "Frame received");

                let outputs = active_stage
                    .process(image, action, &payload)
                    .context("active stage process")?;

                for out in outputs {
                    match out {
                        ActiveStageOutput::ResponseFrame(response) => {
                            writer.write_all(&response).await.context("write response")?;
                        }
                        ActiveStageOutput::GraphicsUpdate(region) => {
                            update_count = update_count.saturating_add(1);
                            if update_count == 1 || update_count.is_multiple_of(50) {
                                info!(
                                    update_count,
                                    left = region.left,
                                    top = region.top,
                                    right = region.right,
                                    bottom = region.bottom,
                                    "Graphics update"
                                );
                            }
                        }
                        ActiveStageOutput::Terminate(reason) => {
                            info!(
                                ?reason,
                                update_count,
                                frame_count,
                                dump_ticks,
                                elapsed_secs = started.elapsed().as_secs(),
                                "Session terminated"
                            );
                            write_frame(image, config, update_count, dump_ticks)?;
                            return Ok(());
                        }
                        ActiveStageOutput::DeactivateAll => {
                            warn!("Server sent Deactivate All; Phase 1 does not reactivate");
                        }
                        ActiveStageOutput::SaveSessionInfo { logon_complete } => {
                            info!(logon_complete, "Save session info");
                        }
                        _ => {}
                    }
                }
            }
            _ = dump.tick() => {
                dump_ticks = dump_ticks.saturating_add(1);
                write_frame(image, config, update_count, dump_ticks)?;
            }
        }
    }
}

/// Decode the session and push the framebuffer to `sink` at `fps` until the
/// server terminates or `stop` resolves (`Ok(reason)` ends the session cleanly).
///
/// Each `input` message is one transaction of browser operations, written to
/// the server as fast-path input between graphics reads.
#[cfg(feature = "modeb-encode")]
pub(super) async fn active_session_encode(
    connection_result: ConnectionResult,
    framed: UpgradedFramed,
    image: &mut DecodedImage,
    fps: u32,
    mut stop: core::pin::Pin<&mut dyn core::future::Future<Output = anyhow::Result<&'static str>>>,
    mut input: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<ironrdp_input::Operation>>>,
    sink: &mut dyn super::encode::EncodeSink,
) -> anyhow::Result<()> {
    let (mut reader, mut writer) = ironrdp_tokio::split_tokio_framed(framed);

    let mut active_stage = ActiveStageBuilder {
        static_channels: connection_result.static_channels,
        user_channel_id: connection_result.user_channel_id,
        io_channel_id: connection_result.io_channel_id,
        message_channel_id: connection_result.message_channel_id,
        share_id: connection_result.share_id,
        compression_type: connection_result.compression_type,
        enable_server_pointer: connection_result.enable_server_pointer,
        pointer_software_rendering: connection_result.pointer_software_rendering,
    }
    .build();

    let frame_period = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut push = interval(frame_period);
    push.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Fire immediately so the first encoded frame is not delayed by one period.
    push.tick().await;

    let mut input_db = ironrdp_input::Database::new();
    let mut update_count: u64 = 0;
    let mut frame_count: u64 = 0;
    let started = Instant::now();

    info!(
        fps,
        input = input.is_some(),
        "Active session started; pushing RGBA to H.264 encoder"
    );

    loop {
        let outputs = tokio::select! {
            frame = reader.read_pdu() => {
                let (action, payload) = frame.context("read PDU")?;
                frame_count = frame_count.saturating_add(1);
                trace!(?action, frame_length = payload.len(), frame_count, "Frame received");

                active_stage
                    .process(image, action, &payload)
                    .context("active stage process")?
            }
            ops = async {
                match input.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => core::future::pending().await,
                }
            } => {
                let events = match ops {
                    Some(ops) => {
                        debug!(?ops, "Applying browser input");
                        input_db.apply(ops)
                    }
                    None => {
                        info!("Browser input channel closed; releasing held keys and buttons");
                        input = None;
                        input_db.release_all()
                    }
                };
                debug!(event_count = events.len(), "Sending fast-path input");
                active_stage
                    .process_fastpath_input(image, &events)
                    .context("encode fast-path input")?
            }
            _ = push.tick() => {
                sink.push_frame(image).context("push frame to encoder")?;
                continue;
            }
            reason = stop.as_mut() => {
                let reason = reason?;
                info!(
                    reason,
                    update_count,
                    frame_count,
                    elapsed_secs = started.elapsed().as_secs(),
                    "Stopping RDP session"
                );
                return Ok(());
            }
        };

        for out in outputs {
            match out {
                ActiveStageOutput::ResponseFrame(response) => {
                    writer.write_all(&response).await.context("write response")?;
                }
                ActiveStageOutput::GraphicsUpdate(region) => {
                    update_count = update_count.saturating_add(1);
                    if update_count == 1 || update_count.is_multiple_of(50) {
                        // Compare with the encode fps: pushing faster than RDP updates only repeats frames.
                        let updates_per_sec = update_count as f64 / started.elapsed().as_secs_f64().max(1e-3);
                        info!(
                            update_count,
                            updates_per_sec = format!("{updates_per_sec:.1}"),
                            left = region.left,
                            top = region.top,
                            right = region.right,
                            bottom = region.bottom,
                            "Graphics update"
                        );
                    }
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!(
                        ?reason,
                        update_count,
                        frame_count,
                        elapsed_secs = started.elapsed().as_secs(),
                        "Session terminated"
                    );
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    warn!("Server sent Deactivate All; Mode B does not reactivate");
                }
                ActiveStageOutput::SaveSessionInfo { logon_complete } => {
                    info!(logon_complete, "Save session info");
                }
                _ => {}
            }
        }
    }
}

fn write_frame(image: &DecodedImage, config: &ModeBConfig, update_count: u64, dump_ticks: u64) -> anyhow::Result<()> {
    framebuffer::write_png(image, &config.frame_path)?;
    info!(
        path = %config.frame_path.display(),
        width = image.width(),
        height = image.height(),
        pixel_format = ?image.pixel_format(),
        update_count,
        dump_ticks,
        "Wrote framebuffer PNG"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fit_desktop_size;

    #[test]
    fn keeps_sizes_that_fit() {
        assert_eq!(fit_desktop_size(1920, 1080), (1920, 1080));
        assert_eq!(fit_desktop_size(1366, 768), (1366, 768));
        assert_eq!(fit_desktop_size(1920, 1200), (1920, 1200));
    }

    #[test]
    fn scales_down_keeping_aspect_ratio() {
        assert_eq!(fit_desktop_size(2560, 1440), (1920, 1080));
        assert_eq!(fit_desktop_size(3840, 2160), (1920, 1080));
        assert_eq!(fit_desktop_size(2560, 1600), (1920, 1200));
        assert_eq!(fit_desktop_size(3440, 1440), (1920, 802));
        assert_eq!(fit_desktop_size(2000, 1500), (1600, 1200));
    }

    #[test]
    fn rounds_down_to_even() {
        assert_eq!(fit_desktop_size(1535, 863), (1534, 862));
    }

    #[test]
    fn clamps_to_minimum() {
        assert_eq!(fit_desktop_size(0, 0), (200, 200));
        assert_eq!(fit_desktop_size(100, 150), (200, 200));
    }
}
