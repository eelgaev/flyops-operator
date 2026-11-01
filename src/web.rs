//! Read-only HTTP surface: health, metrics, dashboard state, and the brain visualisation page.
use crate::state::AppState;
use axum::{extract::State, http::header, response::{Html, IntoResponse}, routing::get, Json, Router};
use prometheus::{Encoder, TextEncoder};
use std::sync::Arc;

const INDEX: &str = include_str!("../web/index.html");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/metrics", get(metrics))
        .route("/api/state", get(api_state))
        .layer(axum::middleware::map_response(|mut r: axum::response::Response| async move {
            let h = r.headers_mut();
            h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
            h.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'".parse().unwrap());
            h.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            r
        }))
        .with_state(state)
}

async fn api_state(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let brains: Vec<_> = s.brains.read().unwrap().values().cloned().collect();
    Json(serde_json::json!({ "brains": brains }))
}

async fn metrics(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let mut buf = vec![];
    TextEncoder::new().encode(&s.metrics.registry.gather(), &mut buf).unwrap();
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], buf)
}
