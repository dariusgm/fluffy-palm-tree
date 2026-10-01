use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::pipelines::import_media::{self, ImportRequest};
use crate::state::AppState;

pub async fn start_import(
    State(state): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let filter = req.filter().map_err(ApiError::BadRequest)?;
    let params = serde_json::to_value(&req).map_err(anyhow::Error::from)?;
    let st = state.clone();
    let job = state
        .jobs
        .start("import_media", params, move |job| {
            import_media::run(st, job, filter)
        })
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job.id, "status_url": format!("/jobs/{}", job.id) })),
    ))
}
