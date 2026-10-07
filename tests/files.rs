mod common;

use axum::http::StatusCode;
use common::{TestEnv, get, post_json, write_png};
use serde_json::json;

async fn index(env: &TestEnv) {
    let (status, resp) = env.call(post_json("/index", json!({ "path": env.root }))).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    env.wait_job(resp["job_id"].as_str().unwrap()).await;
}

async fn count(env: &TestEnv) -> i64 {
    env.state
        .db
        .call(|c| Ok(c.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?))
        .await
        .unwrap()
}

fn populate(env: &TestEnv) {
    write_png(&env.root.join("keep/a.png"), 16, 16);
    write_png(&env.root.join("old/data/b.png"), 16, 16);
    std::fs::write(env.root.join("old/data/c.txt"), "some old text").unwrap();
    write_png(&env.root.join("old/d.png"), 16, 16);
}

fn prefix(env: &TestEnv, rel: &str) -> String {
    env.root.join(rel).to_str().unwrap().to_string()
}

#[tokio::test]
async fn dry_run_is_the_default_and_changes_nothing() {
    let env = TestEnv::new();
    populate(&env);
    index(&env).await;

    let (status, r) = env
        .call(post_json(
            "/files/delete",
            json!({ "path_prefix": prefix(&env, "old"), "delete_source": true }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["dry_run"], true);
    assert_eq!(r["matched"], 3, "{r}");
    assert_eq!(count(&env).await, 4);
    assert!(env.root.join("old/d.png").exists());
}

#[tokio::test]
async fn rejects_empty_and_unsafe_selectors() {
    let env = TestEnv::new();
    populate(&env);
    index(&env).await;
    let root = env.root.to_str().unwrap().to_string();

    for (body, expected) in [
        (json!({}), StatusCode::BAD_REQUEST),
        (json!({ "dry_run": false }), StatusCode::BAD_REQUEST),
        (json!({ "path_prefix": root, "dry_run": false }), StatusCode::BAD_REQUEST),
        (json!({ "path_prefix": format!("{root}/../x") }), StatusCode::BAD_REQUEST),
        (json!({ "path_prefix": "relative/path" }), StatusCode::BAD_REQUEST),
        (json!({ "path_prefix": "/etc" }), StatusCode::FORBIDDEN),
    ] {
        let (status, r) = env.call(post_json("/files/delete", body.clone())).await;
        assert_eq!(status, expected, "{body} -> {r}");
    }
    assert_eq!(count(&env).await, 4);
}

#[tokio::test]
async fn removes_only_records_by_default() {
    let env = TestEnv::new();
    populate(&env);
    index(&env).await;

    let (status, r) = env
        .call(post_json(
            "/files/delete",
            json!({ "path_prefix": prefix(&env, "old/data"), "dry_run": false }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["removed_records"], 2, "{r}");
    assert_eq!(count(&env).await, 2);
    assert!(env.root.join("old/data/b.png").exists());
    assert!(env.root.join("old/data/c.txt").exists());
}

#[tokio::test]
async fn delete_source_removes_files_and_empty_directories_only() {
    let env = TestEnv::new();
    populate(&env);
    std::fs::write(env.root.join("old/data/unindexed.xyz"), b"\x00\x01\x02bin").unwrap();
    index(&env).await;

    let (status, r) = env
        .call(post_json(
            "/files/delete",
            json!({
                "path_prefix": prefix(&env, "old/data"),
                "delete_source": true,
                "dry_run": false
            }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["deleted_source_files"], 2, "{r}");
    assert_eq!(r["error_count"], 0, "{r}");
    assert!(!env.root.join("old/data/b.png").exists());
    // Not indexed, so not selected and not deleted; keeps its directory alive.
    assert!(env.root.join("old/data/unindexed.xyz").exists());
    assert!(env.root.join("old/d.png").exists());
    assert_eq!(count(&env).await, 2);

    // Delete the rest of "old": its directories become empty and are pruned, the root stays.
    std::fs::remove_file(env.root.join("old/data/unindexed.xyz")).unwrap();
    // Only directories on the path of a deleted file are pruned, not unrelated empty ones.
    std::fs::remove_dir(env.root.join("old/data")).unwrap();
    let (_, r) = env
        .call(post_json(
            "/files/delete",
            json!({ "path_prefix": prefix(&env, "old"), "delete_source": true, "dry_run": false }),
        ))
        .await;
    assert_eq!(r["deleted_source_files"], 1, "{r}");
    assert!(!env.root.join("old").exists());
    assert!(env.root.join("keep/a.png").exists());
    assert!(env.root.exists());
}

#[tokio::test]
async fn delete_by_id_and_get_file() {
    let env = TestEnv::new();
    populate(&env);
    index(&env).await;
    let id: String = env
        .state
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT id FROM files WHERE rel_path = 'keep/a.png'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();

    let (status, f) = env.call(get(&format!("/files/{id}"))).await;
    assert_eq!(status, StatusCode::OK, "{f}");
    assert_eq!(f["rel_path"], "keep/a.png");
    assert_eq!(f["kind"], "image");

    let (_, r) = env
        .call(post_json(
            "/files/delete",
            json!({ "ids": [id], "dry_run": false }),
        ))
        .await;
    assert_eq!(r["removed_records"], 1, "{r}");
    let (status, _) = env.call(get(&format!("/files/{id}"))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
