mod common;

use axum::http::StatusCode;
use media_search::db::Db;
use media_search::{AppState, router};

#[tokio::test]
async fn health_allowed_from_private_net() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(
        AppState::new(
            common::test_config(tmp.path(), tmp.path()),
            Db::open_in_memory().unwrap(),
        )
        .unwrap(),
    );
    let (resp, body) = common::send(app, "192.168.178.10:5555", common::get("/health")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn health_forbidden_from_public_ip() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(
        AppState::new(
            common::test_config(tmp.path(), tmp.path()),
            Db::open_in_memory().unwrap(),
        )
        .unwrap(),
    );
    let (resp, _) = common::send(app, "203.0.113.5:5555", common::get("/health")).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
