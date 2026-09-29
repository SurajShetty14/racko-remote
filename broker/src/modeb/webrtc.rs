//! Stream a decoded RDP framebuffer to a browser over WebRTC.
//!
//! `appsrc(RGBA) → videoconvert → nvh264enc → h264parse → rtph264pay → webrtcbin`
//!
//! webrtcbin is the offerer and the browser answers. Signaling is JSON over a
//! WebSocket, with the same messages as GStreamer's [webrtc sendrecv example]:
//! `{"type":"offer"|"answer","sdp":…}` and `{"type":"ice","candidate":…,"sdpMLineIndex":…}`.
//! On the broker's `/modeb/webrtc` route the browser first sends
//! `{"type":"auth","username":…,"password":…,"domain":…}` (see [`super::service`]).
//! The broker also sends `{"type":"error","message":…}` before closing when a session cannot start;
//! credential failures start with [`AUTH_FAILED`].
//! No STUN/TURN is configured, so only host candidates are gathered and the
//! browser must reach the box directly.
//!
//! An ordered `input` data channel (pre-negotiated, id 0) carries browser
//! mouse/keyboard JSON back to the RDP session (see [`super::input`]).
//! webrtcbin creates it before the first offer so the SDP has an `m=application`
//! section; the page creates the matching channel before answering.
//!
//! [`stream_session`] runs one session per signaling WebSocket. The broker's
//! `/modeb/webrtc` route ([`super::service`]) calls it once per browser; the
//! deprecated `modeb_webrtc_probe` ([`run_webrtc`]) calls it once and exits.
//!
//! [webrtc sendrecv example]: https://gitlab.freedesktop.org/gstreamer/gstreamer/-/tree/main/subprojects/gst-examples/webrtc/sendrecv

use core::net::SocketAddr;
use core::pin::pin;
use core::time::Duration;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow, bail};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse as _, Response};
use axum::routing::get;
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_input::Operation;
use ironrdp_session::image::DecodedImage;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::{Instrument as _, Span, debug, info, warn};

use crate::config::{broker_dotenv_path, dotenv_map};
use crate::modeb::ModeBConfig;
use crate::modeb::connect;
use crate::modeb::encode::{EncoderSettings, H264Pipeline};
use crate::modeb::input::InputTranslator;
use crate::registry::Session;

/// Label and pre-negotiated SCTP stream id of the browser input channel (must match the web client).
const INPUT_CHANNEL_LABEL: &str = "input";
const INPUT_CHANNEL_ID: i32 = 0;

const MODEB_HTML: &str = include_str!("../../static/modeb.html");

const DEFAULT_BIND: &str = "127.0.0.1:8080";

/// TCP + TLS + CredSSP + capability exchange with the target VM.
const RDP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Browser-facing error for missing, malformed, or rejected credentials.
pub(super) const AUTH_FAILED: &str = "Authentication failed";

/// Linked after the encoder. The RTP capsfilter lets webrtcbin build the offer
/// before the first encoded buffer reaches it.
const WEBRTC_TAIL: &str = "h264parse name=modeb-parse \
     ! rtph264pay name=modeb-pay config-interval=-1 pt=96 \
     ! application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000 \
     ! webrtcbin name=modeb-webrtc bundle-policy=max-bundle";

/// Elements the WebRTC pipeline needs at runtime, including webrtcbin internals.
const REQUIRED_ELEMENTS: &[&str] = &[
    "appsrc",
    "videoconvert",
    "h264parse",
    "rtph264pay",
    "webrtcbin",
    "rtpbin",
    "nicesrc",
    "nicesink",
    "dtlssrtpenc",
    "srtpenc",
    "sctpenc",
    "sctpdec",
];

/// Settings for the deprecated standalone WebRTC probe.
#[derive(Debug, Clone)]
pub struct WebRtcConfig {
    /// HTTP + signaling listen address (serves `/modeb.html` and `/webrtc`).
    pub bind: SocketAddr,
    pub encoder: EncoderSettings,
}

impl WebRtcConfig {
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        let bind = lookup("MODEB_WEBRTC_BIND")
            .unwrap_or_else(|| DEFAULT_BIND.to_owned())
            .parse()
            .context("MODEB_WEBRTC_BIND must be ip:port")?;

        Ok(Self {
            bind,
            encoder: EncoderSettings::load()?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SignalMsg {
    Offer {
        sdp: String,
    },
    Answer {
        sdp: String,
    },
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    /// Broker to browser only: the session could not start.
    Error {
        message: String,
    },
}

/// Posted from GStreamer threads to the signaling task.
enum PeerEvent {
    Send(SignalMsg),
    Failed(String),
}

/// Hands the first browser WebSocket to the probe; later upgrades are rejected.
type SessionSlot = Arc<Mutex<Option<oneshot::Sender<WebSocket>>>>;

/// Initialize GStreamer and fail with an install hint when WebRTC elements are missing.
pub(super) fn init_gstreamer() -> anyhow::Result<()> {
    gst::init().context("gstreamer init")?;
    let missing: Vec<&str> = REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing GStreamer elements {missing:?}; install gstreamer1.0-nice, \
             gstreamer1.0-plugins-good and gstreamer1.0-plugins-bad"
        );
    }
    Ok(())
}

/// Deprecated standalone probe: serve the test page, stream `RDP_TARGET` to one browser, exit.
///
/// The main broker serves Mode B at `/modeb/webrtc` with per-session targets.
pub async fn run_webrtc(rdp: ModeBConfig, webrtc: WebRtcConfig) -> anyhow::Result<()> {
    warn!("modeb_webrtc_probe is deprecated; the main broker serves Mode B at /modeb/webrtc");
    info!(
        target = %rdp.destination_label(),
        bind = %webrtc.bind,
        settings = ?webrtc.encoder,
        "Mode B WebRTC probe starting (server-side auth)"
    );

    init_gstreamer()?;

    let (socket_tx, socket_rx) = oneshot::channel();
    let slot: SessionSlot = Arc::new(Mutex::new(Some(socket_tx)));
    let app = Router::new()
        .route("/", get(test_page))
        .route("/modeb.html", get(test_page))
        .route("/webrtc", get(signaling_upgrade))
        .with_state(slot);

    let listener = tokio::net::TcpListener::bind(webrtc.bind)
        .await
        .with_context(|| format!("bind {}", webrtc.bind))?;
    let server = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            warn!(error = %err, "Test page server stopped");
        }
    });
    info!(
        url = %format!("http://{}/modeb.html", webrtc.bind),
        "Serving WebRTC test page; open it in a browser on this box"
    );

    let socket = socket_rx
        .await
        .context("test page server stopped before a browser connected")?;
    info!("Browser opened signaling WebSocket; connecting to RDP target");

    let result = stream_session(socket, &rdp, &webrtc.encoder, None).await;
    server.abort();
    result
}

async fn test_page() -> Html<&'static str> {
    Html(MODEB_HTML)
}

async fn signaling_upgrade(State(slot): State<SessionSlot>, ws: WebSocketUpgrade) -> Response {
    let sender = slot.lock().ok().and_then(|mut slot| slot.take());
    let Some(sender) = sender else {
        warn!("Rejected signaling WebSocket; this probe serves a single WebRTC session");
        return (StatusCode::CONFLICT, "a WebRTC session is already active").into_response();
    };

    info!("Upgrading signaling WebSocket");
    ws.on_upgrade(move |socket| async move {
        if sender.send(socket).is_err() {
            warn!("Probe stopped before the signaling WebSocket was handed off");
        }
    })
}

/// Connect to `rdp`, then stream it to the browser on `socket` until the browser
/// leaves or the server ends the session.
///
/// Dropping the returned future tears the session down: the pipeline goes to NULL,
/// webrtcbin, the RDP connection and the socket are released, and the signaling task is aborted.
/// `counters` receives encoded video bytes (received) and browser input bytes (sent).
///
/// The future is not `Send` (IronRDP's connector and framed futures), so await it directly
/// or on a current-thread runtime, not in a multi-thread `tokio::spawn`.
pub(super) async fn stream_session(
    mut socket: WebSocket,
    rdp: &ModeBConfig,
    settings: &EncoderSettings,
    counters: Option<Arc<Session>>,
) -> anyhow::Result<()> {
    let connected = match tokio::time::timeout(RDP_CONNECT_TIMEOUT, connect::connect(rdp)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("timed out after {}s", RDP_CONNECT_TIMEOUT.as_secs())),
    };
    let (connection_result, framed) = match connected {
        Ok(connected) => connected,
        Err(err) => {
            let message = if connect::is_auth_failure(&err) {
                AUTH_FAILED
            } else {
                "Could not connect to the remote desktop"
            };
            send_error(&mut socket, message).await;
            return Err(err.context("Mode B connect"));
        }
    };
    let width = connection_result.desktop_size.width;
    let height = connection_result.desktop_size.height;
    info!(
        width,
        height,
        pixel_format = "RGBA",
        compression = ?connection_result.compression_type,
        "Negotiated session parameters"
    );

    let parts = match build_stream(width, height, settings, counters.as_ref()) {
        Ok(parts) => parts,
        Err(err) => {
            send_error(&mut socket, "Could not start the video stream").await;
            return Err(err);
        }
    };
    let StreamParts {
        mut encoder,
        webrtcbin,
        input_channel,
        input_rx,
        events_rx,
    } = parts;

    let signaling = tokio::spawn(signaling_loop(socket, webrtcbin, events_rx).instrument(Span::current()));
    let _abort_signaling = AbortOnDrop(signaling.abort_handle());
    let stop = pin!(async move {
        match signaling.await {
            Ok(result) => result,
            Err(err) => Err(anyhow!("signaling task failed: {err}")),
        }
    });

    let mut image = DecodedImage::new(PixelFormat::RgbA32, width, height);
    let result = connect::active_session_encode(
        connection_result,
        framed,
        &mut image,
        settings.fps,
        stop,
        Some(input_rx),
        &mut encoder,
    )
    .await
    .context("Mode B WebRTC session");

    info!(frames_pushed = encoder.frames_pushed(), "Mode B WebRTC session ended");
    drop(input_channel);
    drop(encoder);
    result
}

/// Tell the browser why the session did not start, then close the socket.
pub(super) async fn send_error(socket: &mut WebSocket, message: &str) {
    let msg = SignalMsg::Error {
        message: message.to_owned(),
    };
    if let Ok(json) = serde_json::to_string(&msg) {
        let _ = socket.send(Message::Text(json.into())).await;
    }
    let _ = socket.send(Message::Close(None)).await;
}

struct StreamParts {
    encoder: H264Pipeline,
    webrtcbin: gst::Element,
    /// Our reference from `create-data-channel`, held until the session ends.
    input_channel: gst_webrtc::WebRTCDataChannel,
    input_rx: mpsc::UnboundedReceiver<Vec<Operation>>,
    events_rx: mpsc::UnboundedReceiver<PeerEvent>,
}

/// Build the encode + WebRTC pipeline, wire the input channel and signaling callbacks, and start it.
fn build_stream(
    width: u16,
    height: u16,
    settings: &EncoderSettings,
    counters: Option<&Arc<Session>>,
) -> anyhow::Result<StreamParts> {
    let encoder = H264Pipeline::build(width, height, settings, WEBRTC_TAIL, true).context("build WebRTC pipeline")?;
    info!(
        encoder = encoder.encoder_name(),
        hardware = encoder.is_hardware(),
        "Selected H.264 encoder element"
    );

    let webrtcbin = encoder
        .pipeline()
        .by_name("modeb-webrtc")
        .context("pipeline missing modeb-webrtc")?;
    let translator = Arc::new(InputTranslator::new(width, height).context("load keyboard scancode table")?);
    debug!(
        key_codes = translator.key_count(),
        "Loaded web client KeyboardEvent.code to scancode table"
    );

    if let Some(session) = counters {
        count_video_bytes(&encoder, session)?;
    }

    // webrtcbin only creates data channels from READY; doing it before PLAYING
    // puts the channel in the first offer instead of forcing a renegotiation.
    encoder.set_state(gst::State::Ready).context("set pipeline Ready")?;
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let input_channel = connect_input_channels(&webrtcbin, &translator, &input_tx, counters)?;
    drop(input_tx);

    let (events_tx, events_rx) = mpsc::unbounded_channel();
    connect_webrtcbin_signals(&webrtcbin, &events_tx);
    drop(events_tx);

    // PLAYING links webrtcbin's sink pad, which fires on-negotiation-needed.
    encoder.play().context("start WebRTC pipeline")?;

    Ok(StreamParts {
        encoder,
        webrtcbin,
        input_channel,
        input_rx,
        events_rx,
    })
}

/// Count RTP payload bytes leaving the payloader (before SRTP/UDP overhead) as bytes received.
fn count_video_bytes(encoder: &H264Pipeline, session: &Arc<Session>) -> anyhow::Result<()> {
    let pad = encoder
        .pipeline()
        .by_name("modeb-pay")
        .context("pipeline missing modeb-pay")?
        .static_pad("src")
        .context("modeb-pay has no src pad")?;
    let session = Arc::clone(session);
    pad.add_probe(
        gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
        move |_pad, info| {
            let bytes = match &info.data {
                Some(gst::PadProbeData::Buffer(buffer)) => buffer.size(),
                Some(gst::PadProbeData::BufferList(list)) => list.calculate_size(),
                _ => 0,
            };
            session.add_received(u64::try_from(bytes).unwrap_or(u64::MAX));
            gst::PadProbeReturn::Ok
        },
    );
    Ok(())
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Relay SDP/ICE between the browser and webrtcbin until either side ends the session.
async fn signaling_loop(
    mut socket: WebSocket,
    webrtcbin: gst::Element,
    mut events: mpsc::UnboundedReceiver<PeerEvent>,
) -> anyhow::Result<&'static str> {
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                let Some(msg) = incoming else {
                    info!("Browser signaling WebSocket closed");
                    return Ok("browser disconnected");
                };
                match msg.context("signaling WebSocket receive")? {
                    Message::Text(text) => handle_browser_message(&webrtcbin, text.as_str())?,
                    Message::Close(frame) => {
                        info!(?frame, "Browser closed signaling WebSocket");
                        return Ok("browser disconnected");
                    }
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            event = events.recv() => match event {
                Some(PeerEvent::Send(msg)) => {
                    let json = serde_json::to_string(&msg).context("encode signaling message")?;
                    socket
                        .send(Message::Text(json.into()))
                        .await
                        .context("signaling WebSocket send")?;
                }
                Some(PeerEvent::Failed(reason)) => bail!("webrtc failed: {reason}"),
                None => bail!("webrtcbin event channel closed"),
            },
        }
    }
}

fn handle_browser_message(webrtcbin: &gst::Element, text: &str) -> anyhow::Result<()> {
    match serde_json::from_str::<SignalMsg>(text).context("decode signaling message")? {
        SignalMsg::Answer { sdp } => {
            info!(sdp_bytes = sdp.len(), "Received SDP answer; setting remote description");
            debug!(%sdp, "Remote SDP answer");
            let sdp = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).context("parse SDP answer")?;
            let answer = gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Answer, sdp);
            webrtcbin.emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
        }
        SignalMsg::Ice {
            candidate,
            sdp_mline_index,
        } => {
            if candidate.is_empty() {
                debug!("Browser finished gathering ICE candidates");
            } else {
                debug!(sdp_mline_index, %candidate, "Remote ICE candidate");
                webrtcbin.emit_by_name::<()>("add-ice-candidate", &[&sdp_mline_index, &candidate]);
            }
        }
        SignalMsg::Offer { .. } => bail!("unexpected SDP offer from browser; the broker is the offerer"),
        SignalMsg::Error { .. } => bail!("unexpected error message from browser"),
    }
    Ok(())
}

/// Offer on negotiation-needed, trickle local candidates, and log state transitions.
///
/// These callbacks run on GStreamer threads, so each re-enters the session's tracing span.
fn connect_webrtcbin_signals(webrtcbin: &gst::Element, events: &mpsc::UnboundedSender<PeerEvent>) {
    let tx = events.clone();
    let span = Span::current();
    webrtcbin.connect_closure(
        "on-negotiation-needed",
        false,
        glib::closure!(move |webrtcbin: &gst::Element| {
            let _span = span.enter();
            info!("webrtcbin needs negotiation; creating SDP offer");
            let tx = tx.clone();
            let element = webrtcbin.clone();
            let span = span.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let _span = span.enter();
                if let Err(err) = on_offer_created(&element, reply, &tx) {
                    let _ = tx.send(PeerEvent::Failed(format!("{err:#}")));
                }
            });
            webrtcbin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        }),
    );

    let tx = events.clone();
    let span = Span::current();
    webrtcbin.connect_closure(
        "on-ice-candidate",
        false,
        glib::closure!(
            move |_webrtcbin: &gst::Element, sdp_mline_index: u32, candidate: &str| {
                let _span = span.enter();
                debug!(sdp_mline_index, candidate, "Local ICE candidate");
                let _ = tx.send(PeerEvent::Send(SignalMsg::Ice {
                    candidate: candidate.to_owned(),
                    sdp_mline_index,
                }));
            }
        ),
    );

    let span = Span::current();
    webrtcbin.connect_notify(Some("signaling-state"), move |webrtcbin, _| {
        let _span = span.enter();
        let state = webrtcbin.property::<gst_webrtc::WebRTCSignalingState>("signaling-state");
        info!(?state, "WebRTC signaling state changed");
    });

    let span = Span::current();
    webrtcbin.connect_notify(Some("ice-gathering-state"), move |webrtcbin, _| {
        let _span = span.enter();
        let state = webrtcbin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        info!(?state, "ICE gathering state changed");
    });

    let tx = events.clone();
    let span = Span::current();
    webrtcbin.connect_notify(Some("ice-connection-state"), move |webrtcbin, _| {
        let _span = span.enter();
        let state = webrtcbin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        info!(?state, "ICE connection state changed");
        if state == gst_webrtc::WebRTCICEConnectionState::Failed {
            let _ = tx.send(PeerEvent::Failed("ICE connection failed".to_owned()));
        }
    });

    let tx = events.clone();
    let span = Span::current();
    webrtcbin.connect_notify(Some("connection-state"), move |webrtcbin, _| {
        let _span = span.enter();
        let state = webrtcbin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        info!(?state, "Peer connection state changed");
        if state == gst_webrtc::WebRTCPeerConnectionState::Failed {
            let _ = tx.send(PeerEvent::Failed("peer connection failed".to_owned()));
        }
    });
}

/// Create the pre-negotiated `input` channel and accept an in-band one from the
/// browser too (`on-data-channel`); both feed translated operations into `input`.
fn connect_input_channels(
    webrtcbin: &gst::Element,
    translator: &Arc<InputTranslator>,
    input: &mpsc::UnboundedSender<Vec<Operation>>,
    counters: Option<&Arc<Session>>,
) -> anyhow::Result<gst_webrtc::WebRTCDataChannel> {
    let options = gst::Structure::builder("input-channel-options")
        .field("ordered", true)
        .field("negotiated", true)
        .field("id", INPUT_CHANNEL_ID)
        .build();
    let channel = webrtcbin
        .emit_by_name::<Option<gst_webrtc::WebRTCDataChannel>>("create-data-channel", &[&INPUT_CHANNEL_LABEL, &options])
        .context("webrtcbin did not create the input data channel")?;
    debug!(
        label = INPUT_CHANNEL_LABEL,
        id = INPUT_CHANNEL_ID,
        "Created pre-negotiated input data channel"
    );
    attach_input_channel(&channel, translator, input, counters);

    let translator = Arc::clone(translator);
    let input = input.clone();
    let counters = counters.cloned();
    let span = Span::current();
    webrtcbin.connect_closure(
        "on-data-channel",
        false,
        glib::closure!(
            move |_webrtcbin: &gst::Element, channel: &gst_webrtc::WebRTCDataChannel| {
                let _span = span.enter();
                let label = channel.label();
                if label.as_deref() == Some(INPUT_CHANNEL_LABEL) {
                    info!(?label, "Browser opened an in-band input data channel");
                    attach_input_channel(channel, &translator, &input, counters.as_ref());
                } else {
                    warn!(?label, "Ignoring unexpected data channel from browser");
                }
            }
        ),
    );

    Ok(channel)
}

fn attach_input_channel(
    channel: &gst_webrtc::WebRTCDataChannel,
    translator: &Arc<InputTranslator>,
    input: &mpsc::UnboundedSender<Vec<Operation>>,
    counters: Option<&Arc<Session>>,
) {
    let span = Span::current();
    channel.connect_on_open(move |channel| {
        let _span = span.enter();
        info!(label = ?channel.label(), "Input data channel open");
    });
    let span = Span::current();
    channel.connect_on_close(move |channel| {
        let _span = span.enter();
        info!(label = ?channel.label(), "Input data channel closed");
    });
    let span = Span::current();
    channel.connect_on_error(move |channel, err| {
        let _span = span.enter();
        warn!(label = ?channel.label(), error = %err, "Input data channel error");
    });

    let translator = Arc::clone(translator);
    let input = input.clone();
    let counters = counters.cloned();
    let span = Span::current();
    channel.connect_on_message_string(move |_channel, msg| {
        let Some(msg) = msg else {
            return;
        };
        if let Some(session) = &counters {
            session.add_sent(u64::try_from(msg.len()).unwrap_or(u64::MAX));
        }
        match translator.translate(msg) {
            Ok(ops) if ops.is_empty() => {}
            Ok(ops) => {
                let _ = input.send(ops);
            }
            Err(err) => {
                let _span = span.enter();
                warn!(error = %format!("{err:#}"), msg_len = msg.len(), "Ignoring malformed input message");
            }
        }
    });
}

fn on_offer_created(
    webrtcbin: &gst::Element,
    reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    tx: &mpsc::UnboundedSender<PeerEvent>,
) -> anyhow::Result<()> {
    let reply = match reply {
        Ok(Some(reply)) => reply,
        Ok(None) => bail!("create-offer returned no reply"),
        Err(err) => bail!("create-offer failed: {err:?}"),
    };
    let offer = reply
        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
        .map_err(|err| anyhow!("create-offer reply has no offer: {err}"))?;

    let sdp = offer.sdp().as_text().context("serialize SDP offer")?;
    info!(
        sdp_bytes = sdp.len(),
        "Created SDP offer; sending it and setting local description"
    );
    debug!(%sdp, "Local SDP offer");

    // Queue the offer before set-local-description starts ICE gathering so the
    // browser never receives a candidate ahead of the offer.
    tx.send(PeerEvent::Send(SignalMsg::Offer { sdp }))
        .map_err(|_| anyhow!("signaling task is gone"))?;
    webrtcbin.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
    Ok(())
}
