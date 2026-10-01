mod common;

use axum::http::StatusCode;
use common::{TestEnv, get, post_json, write_pdf, write_png, write_video};
use serde_json::json;

fn populate(env: &TestEnv) {
    write_png(&env.root.join("a.png"), 32, 16);
    write_png(&env.root.join("sub/b.png"), 8, 300);
    write_png(&env.root.join(".hidden/c.png"), 8, 8);
    std::fs::write(env.root.join("notes.txt"), "the quick brown fox").unwrap();
    std::fs::write(env.root.join("readme.md"), "# Title\nmarkdown body").unwrap();
    std::fs::write(env.root.join("ignored.xyz"), "nope").unwrap();
}

async fn index(env: &TestEnv, body: serde_json::Value) -> serde_json::Value {
    let (status, resp) = env.call(post_json("/index", body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    env.wait_job(resp["job_id"].as_str().unwrap()).await
}

async fn query_i64(env: &TestEnv, sql: &'static str) -> i64 {
    env.state
        .db
        .call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn indexes_tree_and_skips_unchanged() {
    let env = TestEnv::new();
    populate(&env);

    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(job["found"], 4, "{job}");
    assert_eq!(job["processed"], 4, "{job}");
    assert_eq!(job["failed"], 0, "{job}");
    assert!(env.staging_is_empty(), "staging copies must be removed");

    assert_eq!(query_i64(&env, "SELECT height FROM images i JOIN files f ON f.id = i.file_id WHERE f.rel_path = 'sub/b.png'").await, 300);
    assert_eq!(
        query_i64(
            &env,
            "SELECT count(*) FROM documents WHERE content LIKE '%quick brown%'"
        )
        .await,
        1
    );
    assert_eq!(
        query_i64(
            &env,
            "SELECT count(*) FROM documents WHERE doc_type = 'markdown'"
        )
        .await,
        1
    );
    assert_eq!(query_i64(&env, "SELECT count(*) FROM files WHERE sha256 IS NOT NULL AND mode > 0 AND summary_status = 'pending'").await, 4);

    let again = index(&env, json!({ "path": env.root })).await;
    assert_eq!(again["skipped"], 4, "{again}");
    assert_eq!(again["processed"], 0, "{again}");
}

#[tokio::test]
async fn traverse_false_only_indexes_top_level() {
    let env = TestEnv::new();
    populate(&env);
    let job = index(&env, json!({ "path": env.root, "traverse": false })).await;
    assert_eq!(job["found"], 3, "{job}");
}

#[tokio::test]
async fn copy_on_index_false_reads_in_place() {
    let env = TestEnv::with_config(|c| c.staging.copy_on_index = false);
    populate(&env);
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["processed"], 4, "{job}");
    assert_eq!(
        query_i64(&env, "SELECT count(*) FROM files WHERE sha256 IS NULL").await,
        4
    );
}

#[tokio::test]
async fn rejects_paths_outside_roots() {
    let env = TestEnv::new();
    let (status, _) = env
        .call(post_json("/index", json!({ "path": "/etc" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env
        .call(post_json("/index", json!({ "path": "/does/not/exist" })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn failed_files_are_reported() {
    let env = TestEnv::new();
    std::fs::write(env.root.join("broken.png"), "not an image").unwrap();
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["failed"], 1, "{job}");
    assert!(
        job["recent_errors"][0]["path"]
            .as_str()
            .unwrap()
            .ends_with("broken.png")
    );
}

#[tokio::test]
async fn jobs_endpoints() {
    let env = TestEnv::new();
    populate(&env);
    let job = index(&env, json!({ "path": env.root })).await;
    let (status, list) = env.call(get("/jobs")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["id"], job["id"]);
    let (status, _) = env.call(get("/jobs/unknown")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let del = axum::http::Request::delete(format!("/jobs/{}", job["id"].as_str().unwrap()))
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, _) = env.call(del).await;
    assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn indexes_pdf_when_poppler_installed() {
    if !media_search::extract::tool_available("pdftotext") {
        eprintln!("skipping: pdftotext not installed");
        return;
    }
    let env = TestEnv::new();
    write_pdf(&env.root.join("doc.pdf"), "Invoice Number 4711");
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["processed"], 1, "{job}");
    assert_eq!(query_i64(&env, "SELECT count(*) FROM documents WHERE doc_type = 'pdf' AND content LIKE '%Invoice Number 4711%' AND page_count = 1").await, 1);
}

#[tokio::test]
async fn indexes_video_when_ffmpeg_installed() {
    let env = TestEnv::new();
    if !write_video(&env.root.join("clip.mp4"), 3) {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["processed"], 1, "{job}");
    assert_eq!(
        query_i64(&env, "SELECT width FROM videos WHERE video_codec = 'h264'").await,
        320
    );
    assert_eq!(
        query_i64(&env, "SELECT round(duration_secs)::BIGINT FROM videos").await,
        3
    );
}
