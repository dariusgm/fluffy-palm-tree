use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::jobs::JobView;
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
