//! HTTP management API and Prometheus text exposition, separate from the relay.
//! Also hosts the Mode B signaling route when the broker is built with it.

use core::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::json;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use crate::registry::{Registry, SessionView};

#[derive(Clone)]
pub struct ApiState {
    pub registry: Arc<Registry>,
    pub metrics: PrometheusHandle,
}

/// `extra` is merged in as-is (Mode B signaling); handlers may extract `ConnectInfo<SocketAddr>`.
pub async fn serve(addr: SocketAddr, state: ApiState, extra: Router) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let app = Router::new()
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/kill", post(kill_session))
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .layer(CorsLayer::permissive())
        .with_state(state)
        .merge(extra);
    tracing::info!(bind = %addr, "management API listening");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn list_sessions(State(state): State<ApiState>) -> Json<Vec<SessionView>> {
    Json(state.registry.list())
}

async fn kill_session(State(state): State<ApiState>, Path(id): Path<String>) -> impl IntoResponse {
    let Ok(id) = Uuid::parse_str(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        );
    };
    if state.registry.kill(id) {
        (StatusCode::OK, Json(json!({ "id": id })))
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        )
    }
}

async fn metrics(State(state): State<ApiState>) -> impl IntoResponse {
    let body = state.metrics.render();
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
        )],
        body,
    )
}
