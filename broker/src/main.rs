//! RDCleanPath WebSocket-to-RDP broker entrypoint.

#![allow(unused_crate_dependencies)] // thin binary; deps are exercised via the `broker` library

use std::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use broker::api::ApiState;
use broker::config::Config;
use broker::registry::Registry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::try_new("info").expect("static info filter is valid"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = Arc::new(Config::load()?);
    let metrics = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|err| anyhow::anyhow!("install prometheus recorder: {err}"))?;
    describe_metrics();

    let registry = Arc::new(Registry::new());
    let management_bind = config.management_bind;
    let api_state = ApiState {
        registry: Arc::clone(&registry),
        metrics,
    };
    let modeb_routes = modeb_routes(&config, &registry).await;
    tokio::spawn(async move {
        if let Err(err) = broker::api::serve(management_bind, api_state, modeb_routes).await {
            tracing::error!(error = %err, "management API stopped");
        }
    });

    let listener = TcpListener::bind(config.bind).await?;
    info!(
        bind = %config.bind,
        management = %config.management_bind,
        target = %config.target_label(),
        tls_insecure = config.tls_insecure,
        idle_timeout_secs = config.idle_timeout.as_secs(),
        "RDCleanPath broker listening"
    );

    let next_id = AtomicU64::new(1);
    loop {
        let (tcp, peer) = listener.accept().await?;
        let config = Arc::clone(&config);
        let registry = Arc::clone(&registry);
        let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            broker::session::handle_connection(tcp, peer, config, registry, conn_id).await;
        });
    }
}

/// Mode B signaling routes, or none when Mode B cannot start (Mode C keeps running).
///
/// Mode B needs a working Cloudflare TURN key: one set of credentials is minted here so a wrong
/// key or token disables Mode B at startup instead of failing every browser.
#[cfg(feature = "modeb-encode")]
async fn modeb_routes(config: &Arc<Config>, registry: &Arc<Registry>) -> axum::Router {
    use anyhow::Context as _;
    use broker::modeb::{
        CredentialResolver, EnvCredentials, ModeBService, ModeBServiceConfig, SIGNALING_PATH, TurnMinter,
    };

    let fallback = EnvCredentials::load();
    let fallback_enabled = fallback.is_some();
    let service = async {
        let settings = ModeBServiceConfig::load()?;
        let turn = TurnMinter::load()?;
        turn.mint().await.context("mint test TURN credentials")?;
        let turn_ttl_secs = turn.ttl_secs();
        let fallback = fallback.map(|credentials| Arc::new(credentials) as Arc<dyn CredentialResolver>);
        let service = ModeBService::new(Arc::clone(config), settings, Arc::clone(registry), fallback, turn)?;
        anyhow::Ok((service, turn_ttl_secs))
    }
    .await;
    match service {
        Ok((service, turn_ttl_secs)) => {
            info!(
                path = SIGNALING_PATH,
                max_sessions = service.settings().max_sessions,
                encoder = ?service.settings().encoder,
                turn_ttl_secs,
                fallback_credentials = fallback_enabled,
                "Mode B WebRTC signaling enabled"
            );
            if fallback_enabled {
                tracing::warn!(
                    "Mode B signs in with RDP_USERNAME/RDP_PASSWORD when a browser sends no credentials; \
                     unset them to require per-session credentials"
                );
            }
            Arc::new(service).router()
        }
        Err(err) => {
            tracing::error!(error = format!("{err:#}"), "Mode B disabled; Mode C keeps running");
            axum::Router::new()
        }
    }
}

#[cfg(not(feature = "modeb-encode"))]
async fn modeb_routes(_config: &Arc<Config>, _registry: &Arc<Registry>) -> axum::Router {
    info!("Mode B not built in; rebuild with --features modeb-encode to serve /modeb/webrtc");
    axum::Router::new()
}

fn describe_metrics() {
    metrics::describe_gauge!(
        "racko_active_sessions",
        "Number of active sessions (Mode C relays and Mode B streams)"
    );
    metrics::describe_counter!(
        "racko_sessions_total",
        "Sessions started (Mode C relays and Mode B streams)"
    );
    metrics::describe_counter!("racko_bytes_sent_total", "Bytes written from clients to RDP servers");
    metrics::describe_counter!(
        "racko_bytes_received_total",
        "Bytes read from RDP servers toward clients"
    );
    metrics::describe_histogram!(
        "racko_session_duration_seconds",
        "Length of an RDP relay session, recorded when it ends"
    );
    metrics::gauge!("racko_active_sessions").set(0.0);
    metrics::counter!("racko_sessions_total").absolute(0);
    metrics::counter!("racko_bytes_sent_total").absolute(0);
    metrics::counter!("racko_bytes_received_total").absolute(0);
}
