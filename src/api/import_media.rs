use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;

use crate::error::ApiError;
use crate::pipelines::import_media::{self, ImportRequest};
use crate::state::AppState;

pub async fn start_import(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let job_id = spawn(&state, req).await?;
    Ok(super::index::accepted(&job_id))
}

pub(super) async fn spawn(state: &AppState, req: ImportRequest) -> Result<String, ApiError> {
    let filter = req.filter().map_err(ApiError::BadRequest)?;
    let params = serde_json::to_value(&req).map_err(anyhow::Error::from)?;
    let st = state.clone();
    let job = state
        .jobs
        .start("import_media", params, move |job| {
            import_media::run(st, job, filter)
        })
        .await?;
    Ok(job.id.clone())
}
