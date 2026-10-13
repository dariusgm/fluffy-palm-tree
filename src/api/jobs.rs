use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::jobs::JobView;
use crate::pipelines::import_media::ImportRequest;
use crate::pipelines::index::IndexRequest;
use crate::state::AppState;

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<JobView>>, ApiError> {
    Ok(Json(state.jobs.list().await?))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<JobView>, ApiError> {
    state
        .jobs
        .get(&id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::NotFound("job not found".into()))
}

pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if state.jobs.cancel(&id) {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "cancelling" })),
        ));
    }
    match state.jobs.get(&id).await? {
        Some(_) => Err(ApiError::Conflict("job is not running".into())),
        None => Err(ApiError::NotFound("job not found".into())),
    }
}

pub async fn pause(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if state.jobs.pause(&id) {
        return Ok((StatusCode::ACCEPTED, Json(json!({ "status": "pausing" }))));
    }
    match state.jobs.get(&id).await? {
        Some(_) => Err(ApiError::Conflict("job is not running".into())),
        None => Err(ApiError::NotFound("job not found".into())),
    }
}

/// Starts a new job with the parameters of a paused, cancelled or interrupted one.
/// Finished work is skipped by the pipelines, so it continues where the old job stopped.
pub async fn resume(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let old = state
        .jobs
        .get(&id)
        .await?
        .ok_or_else(|| ApiError::NotFound("job not found".into()))?;
    if !matches!(old.status.as_str(), "paused" | "cancelled" | "interrupted") {
        return Err(ApiError::Conflict(format!(
            "job is {}, only paused, cancelled or interrupted jobs can be resumed",
            old.status
        )));
    }
    if state.jobs.is_running(&old.kind, &old.params) {
        return Err(ApiError::Conflict(
            "a job with the same parameters is already running".into(),
        ));
    }
    let bad = |e: serde_json::Error| ApiError::BadRequest(format!("stored job params: {e}"));
    let job_id = match old.kind.as_str() {
        "index" => {
            let req: IndexRequest = serde_json::from_value(old.params).map_err(bad)?;
            super::index::spawn(&state, req).await?
        }
        "import_media" => {
            let req: ImportRequest = serde_json::from_value(old.params).map_err(bad)?;
            super::import_media::spawn(&state, req).await?
        }
        other => {
            return Err(ApiError::BadRequest(format!("cannot resume job kind {other}")));
        }
    };
    Ok(super::index::accepted(&job_id))
}
