mod health;
mod index;
mod jobs;

use axum::Router;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::security::{IpAllowlist, ip_allowlist};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let allow = IpAllowlist::new(state.config.server.allowed_cidrs.clone());
    let max_body = state.config.server.max_body_bytes;

    Router::new()
        .route("/health", get(health::health))
        .route("/index", post(index::start_index))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::get).delete(jobs::cancel))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(middleware::from_fn_with_state(allow, ip_allowlist))
        .layer(TraceLayer::new_for_http())
}
