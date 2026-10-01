use axum::Json;
use axum::extract::State;

use crate::error::ApiError;
use crate::search::{self, SearchResponse, query::SearchRequest};
use crate::state::AppState;

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let resp = search::search(&state.db, req)
        .await
        .map_err(ApiError::BadRequest)??;
    Ok(Json(resp))
}
