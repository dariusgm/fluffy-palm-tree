use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::state::AppState;

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

pub async fn llm(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let configured = state.llm.model().to_string();
    match state.llm.models().await {
        Ok(models) => {
            let available: Vec<String> = models["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let vision = state.llm.vision_supported().await;
            (
                StatusCode::OK,
                Json(json!({
                    "reachable": true,
                    "configured_model": configured,
                    "configured_model_loaded": available.contains(&configured),
                    "models": available,
                    "vision": vision,
                })),
            )
        }
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({ "reachable": false, "configured_model": configured, "error": format!("{e:#}") }),
            ),
        ),
    }
}
