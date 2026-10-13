use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::pipelines::index::{self, IndexRequest};
use crate::security::resolve_in_roots;
use crate::state::AppState;

pub async fn start_index(
    State(state): State<AppState>,
    Json(req): Json<IndexRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let job_id = spawn(&state, req).await?;
    Ok(accepted(&job_id))
}

pub(super) async fn spawn(state: &AppState, req: IndexRequest) -> Result<String, ApiError> {
    let roots = state.config.roots.clone();
    let requested = req.path.clone();
    let target = tokio::task::spawn_blocking(move || resolve_in_roots(&roots, &requested))
        .await
        .map_err(anyhow::Error::from)??;

    let params = json!({ "path": target.path, "traverse": req.traverse });
    let traverse = req.traverse;
    let st = state.clone();
    let job = state
        .jobs
        .start("index", params, move |job| {
            index::run(st, job, target, traverse)
        })
        .await?;
    Ok(job.id.clone())
}

pub(super) fn accepted(job_id: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::ACCEPTED,
        Json(json!({ "job_id": job_id, "status_url": format!("/jobs/{job_id}") })),
    )
}
