mod files;
mod health;
mod import_media;
mod index;
mod jobs;
mod search;

use axum::Router;
use axum::middleware;
use axum::http::header;
use axum::response::Html;
use axum::routing::{get, post};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::security::{IpAllowlist, ip_allowlist};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let allow = IpAllowlist::new(state.config.server.allowed_cidrs.clone());
    let max_body = state.config.server.max_body_bytes;

    Router::new()
        .route("/", get(|| async { Html(include_str!("../../web/index.html")) }))
        .route(
            "/assets/bootstrap.min.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css")],
                    include_str!("../../web/vendor/bootstrap.min.css"),
                )
            }),
        )
        .route(
            "/assets/bootstrap.bundle.min.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript")],
                    include_str!("../../web/vendor/bootstrap.bundle.min.js"),
                )
            }),
        )
        .route("/health", get(health::health))
        .route("/health/llm", get(health::llm))
        .route("/index", post(index::start_index))
        .route("/import_media", post(import_media::start_import))
        .route("/search", post(search::search))
        .route("/files/delete", post(files::delete_files))
        .route("/files/{id}", get(files::get_file))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::get).delete(jobs::cancel))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(middleware::from_fn_with_state(allow, ip_allowlist))
        .layer(TraceLayer::new_for_http())
}
