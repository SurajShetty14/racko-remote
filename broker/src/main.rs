mod api;
mod config;
mod registry;
mod session;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::api::ApiState;
use crate::config::Config;
use crate::registry::Registry;

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
    tokio::spawn(async move {
        if let Err(err) = api::serve(management_bind, api_state).await {
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
            session::handle_connection(tcp, peer, config, registry, conn_id).await;
        });
    }
}

fn describe_metrics() {
    metrics::describe_gauge!("racko_active_sessions", "Number of active RDP relay sessions");
    metrics::describe_counter!("racko_sessions_total", "RDP relay sessions started");
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
