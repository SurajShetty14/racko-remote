//! Mode B in the main broker: each WebSocket to `GET /modeb/webrtc?dest=host:port`
//! runs its own RDP connection, encode pipeline, and WebRTC peer.
//!
//! `dest` must pass the same allow-list as the Mode C relay ([`Config::allows`]).
//! The browser's first message carries the RDP credentials for this session (never the URL):
//! `{"type":"auth","username":…,"password":…,"domain":…}`. When it sends an empty username and
//! password, the optional fallback [`CredentialResolver`] is used; otherwise the session is refused.
//! Each accepted session then mints its own Cloudflare TURN credentials ([`TurnMinter`]).
//! Sessions register in the shared [`Registry`] as mode B, so the management API lists
//! and kills them like relays.

use core::net::SocketAddr;
use core::time::Duration;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::response::Response;
use axum::routing::get;
use serde::Deserialize;
use tokio::sync::{Semaphore, oneshot};
use tracing::{Instrument as _, error, info, info_span, warn};

use crate::config::{Config, broker_dotenv_path, dotenv_map, split_host_port};
use crate::modeb::encode::EncoderSettings;
use crate::modeb::webrtc::{AUTH_FAILED, init_gstreamer, send_error, stream_session};
use crate::modeb::{CredentialResolver, ModeBConfig, RdpCredentials, TurnMinter};
use crate::registry::{Registry, SessionMode};

/// Signaling route on the management HTTP server.
pub const SIGNALING_PATH: &str = "/modeb/webrtc";

/// How long the browser has to send its `auth` message after the WebSocket opens.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// GeForce NVENC allows roughly 5-8 concurrent encode sessions, depending on the driver.
const DEFAULT_MAX_SESSIONS: usize = 5;
const DEFAULT_DESKTOP_WIDTH: u16 = 1280;
const DEFAULT_DESKTOP_HEIGHT: u16 = 1024;

/// Broker-hosted Mode B settings.
#[derive(Debug, Clone)]
pub struct ModeBServiceConfig {
    /// `MODEB_MAX_SESSIONS`: sessions beyond this are rejected before touching the GPU.
    pub max_sessions: usize,
    /// `MODEB_DESKTOP_WIDTH` / `MODEB_DESKTOP_HEIGHT`: requested RDP desktop size.
    pub desktop_width: u16,
    pub desktop_height: u16,
    pub encoder: EncoderSettings,
}

impl ModeBServiceConfig {
    pub fn load() -> anyhow::Result<Self> {
        let file = dotenv_map(broker_dotenv_path());
        let lookup = |key: &str| std::env::var(key).ok().or_else(|| file.get(key).cloned());

        let max_sessions: usize = lookup("MODEB_MAX_SESSIONS")
            .map_or(Ok(DEFAULT_MAX_SESSIONS), |v| v.parse())
            .context("MODEB_MAX_SESSIONS must be a positive integer")?;
        if max_sessions == 0 {
            bail!("MODEB_MAX_SESSIONS must be >= 1");
        }
        let desktop_width = lookup("MODEB_DESKTOP_WIDTH")
            .map_or(Ok(DEFAULT_DESKTOP_WIDTH), |v| v.parse())
            .context("MODEB_DESKTOP_WIDTH must be a u16")?;
        let desktop_height = lookup("MODEB_DESKTOP_HEIGHT")
            .map_or(Ok(DEFAULT_DESKTOP_HEIGHT), |v| v.parse())
            .context("MODEB_DESKTOP_HEIGHT must be a u16")?;

        Ok(Self {
            max_sessions,
            desktop_width,
            desktop_height,
            encoder: EncoderSettings::load()?,
        })
    }
}

pub struct ModeBService {
    config: Arc<Config>,
    settings: ModeBServiceConfig,
    registry: Arc<Registry>,
    fallback_credentials: Option<Arc<dyn CredentialResolver>>,
    turn: TurnMinter,
    slots: Arc<Semaphore>,
}

impl ModeBService {
    /// Fails when GStreamer or its WebRTC plugins are missing, so the caller can keep
    /// serving Mode C without Mode B.
    ///
    /// `fallback_credentials` signs in for browsers that send no credentials; with `None`,
    /// every session needs browser credentials. `turn` mints each session's STUN/TURN servers.
    pub fn new(
        config: Arc<Config>,
        settings: ModeBServiceConfig,
        registry: Arc<Registry>,
        fallback_credentials: Option<Arc<dyn CredentialResolver>>,
        turn: TurnMinter,
    ) -> anyhow::Result<Self> {
        init_gstreamer()?;
        let slots = Arc::new(Semaphore::new(settings.max_sessions));
        Ok(Self {
            config,
            settings,
            registry,
            fallback_credentials,
            turn,
            slots,
        })
    }

    pub fn settings(&self) -> &ModeBServiceConfig {
        &self.settings
    }

    /// `GET /modeb/webrtc`; serve with connect info so the client IP reaches the registry.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(SIGNALING_PATH, get(signaling_upgrade))
            .with_state(self)
    }

    fn active_sessions(&self) -> usize {
        self.settings.max_sessions - self.slots.available_permits()
    }

    async fn serve_socket(self: Arc<Self>, mut socket: WebSocket, peer: SocketAddr, dest: Option<String>) {
        let client_ip = peer.ip().to_string();
        let dest = dest.unwrap_or_default();
        let max_sessions = self.settings.max_sessions;

        let (host, port) = match split_host_port(&dest) {
            Ok(parsed) => parsed,
            Err(err) => {
                warn!(%client_ip, %dest, error = %err, "Rejected Mode B session with a bad destination");
                send_error(&mut socket, "Destination must be host:port").await;
                return;
            }
        };
        if !self.config.allows(&host, port) {
            warn!(
                %client_ip,
                %dest,
                allowed = %self.config.target_label(),
                "Rejected Mode B session: destination is not an allowed RDP target"
            );
            send_error(&mut socket, "Destination is not allowed").await;
            return;
        }

        // Before taking a GPU slot, so a browser without credentials never holds one.
        let resolved: anyhow::Result<(RdpCredentials, &'static str)> = async {
            let hello = tokio::time::timeout(AUTH_TIMEOUT, async {
                loop {
                    let msg = socket
                        .recv()
                        .await
                        .context("browser closed the socket before sending credentials")?
                        .context("signaling WebSocket receive")?;
                    match msg {
                        // The parse error is dropped: it can quote field values.
                        Message::Text(text) => {
                            break serde_json::from_str::<ClientHello>(text.as_str())
                                .map_err(|_| anyhow!("first message is not an auth message"));
                        }
                        Message::Close(_) => bail!("browser closed the socket before sending credentials"),
                        Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    }
                }
            })
            .await
            .map_err(|_| anyhow!("no auth message within {}s", AUTH_TIMEOUT.as_secs()))??;

            let ClientHello::Auth {
                username,
                password,
                domain,
            } = hello;
            let username = username.trim().to_owned();
            match (username.is_empty(), password.is_empty()) {
                (false, false) => {
                    let domain = domain
                        .map(|domain| domain.trim().to_owned())
                        .filter(|domain| !domain.is_empty());
                    Ok((
                        RdpCredentials {
                            username,
                            password,
                            domain,
                        },
                        "browser",
                    ))
                }
                (true, true) => {
                    let fallback = self
                        .fallback_credentials
                        .as_ref()
                        .context("browser sent no credentials and no fallback is configured")?;
                    let credentials = fallback
                        .resolve(&host, port)
                        .await
                        .context("fallback credential lookup")?;
                    Ok((credentials, "fallback"))
                }
                _ => bail!("browser sent a username or a password, but not both"),
            }
        }
        .await;
        let (credentials, source) = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                warn!(%client_ip, %dest, reason = format!("{err:#}"), "Rejected Mode B session: authentication failed");
                send_error(&mut socket, AUTH_FAILED).await;
                return;
            }
        };

        let Ok(permit) = Arc::clone(&self.slots).try_acquire_owned() else {
            error!(
                %client_ip,
                %dest,
                max_sessions,
                "Rejected Mode B session: concurrent session limit reached (MODEB_MAX_SESSIONS bounds NVENC use)"
            );
            send_error(&mut socket, "All GPU streams are in use; try again later").await;
            return;
        };

        let ice = match self.turn.mint().await {
            Ok(ice) => ice,
            Err(err) => {
                error!(%client_ip, %dest, error = format!("{err:#}"), "Rejected Mode B session: TURN credential minting failed");
                send_error(&mut socket, "Could not get relay (TURN) credentials; try again later").await;
                return;
            }
        };

        let username = credentials.username.clone();
        let label = format!("{host}:{port}");
        let rdp = ModeBConfig::for_target(
            host,
            port,
            credentials,
            self.config.tls_insecure,
            self.settings.desktop_width,
            self.settings.desktop_height,
        );
        let live = self.registry.register(SessionMode::B, label.clone(), client_ip.clone());
        let session_id = live.session.id;
        let span = info_span!("modeb", %session_id, dest = %label);
        span.in_scope(|| {
            info!(
                %client_ip,
                %username,
                credentials = source,
                turn_servers = ice.turn_count(),
                active = self.active_sessions(),
                max_sessions,
                "Mode B session starting"
            );
        });

        // Each session gets its own thread and current-thread runtime: the IronRDP connect and
        // framed futures are not `Send`, and a panic in one session cannot reach the others.
        let (done_tx, done_rx) = oneshot::channel();
        let counters = Arc::clone(&live.session);
        let mut kill_rx = live.kill_rx.clone();
        let encoder = self.settings.encoder;
        let thread_span = span.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("modeb-{}", session_id.simple()))
            .spawn(move || {
                let session = async move {
                    tokio::select! {
                        result = stream_session(socket, &rdp, &encoder, &ice, Some(counters)) => result,
                        _ = kill_rx.wait_for(|killed| *killed) => {
                            info!("Mode B session killed from the management API");
                            Ok(())
                        }
                    }
                };
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("build Mode B session runtime")
                    .and_then(|runtime| runtime.block_on(session.instrument(thread_span)));
                let _ = done_tx.send(result);
            });
        let result = match spawned {
            Ok(_thread) => done_rx
                .await
                .unwrap_or_else(|_| Err(anyhow!("Mode B session thread panicked"))),
            Err(err) => Err(anyhow::Error::new(err).context("spawn Mode B session thread")),
        };

        span.in_scope(|| match result {
            Ok(()) => info!("Mode B session ended"),
            Err(err) => error!(
                error = format!("{err:#}"),
                active = self.active_sessions(),
                max_sessions,
                "Mode B session failed"
            ),
        });
        drop(live);
        drop(permit);
    }
}

/// First browser message on the signaling socket. No `Debug`: it carries the password.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientHello {
    Auth {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        domain: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct SignalingQuery {
    /// RDP destination as `host:port`.
    dest: Option<String>,
}

async fn signaling_upgrade(
    State(service): State<Arc<ModeBService>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<SignalingQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| service.serve_socket(socket, peer, query.dest))
}
