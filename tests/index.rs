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
        query_i64(&env, "SELECT count(*) FROM files WHERE length(sha256) = 64").await,
        4,
        "hash is computed even without a staging copy"
    );
    assert!(env.staging_is_empty());
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

async fn query_str(env: &TestEnv, sql: &'static str) -> String {
    env.state
        .db
        .call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

/// Sets the file's mtime to a fixed value so a re-index sees a stat change.
fn set_mtime(path: &std::path::Path, secs: i64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
        .unwrap();
}

#[tokio::test]
async fn touched_file_keeps_record_and_llm_results() {
    let env = TestEnv::new();
    let file = env.root.join("a.png");
    write_png(&file, 32, 16);
    index(&env, json!({ "path": env.root })).await;
    let id = query_str(&env, "SELECT id FROM files").await;
    env.state
        .db
        .call(|c| {
            Ok(c.execute(
                "UPDATE files SET summary = 'llm text', summary_status = 'done'",
                [],
            )?)
        })
        .await
        .unwrap();

    set_mtime(&file, 1_600_000_000);
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(
        (job["processed"].as_u64(), job["skipped"].as_u64()),
        (Some(0), Some(1)),
        "{job}"
    );
    assert_eq!(
        query_str(&env, "SELECT id FROM files").await,
        id,
        "same record"
    );
    assert_eq!(
        query_str(&env, "SELECT summary FROM files").await,
        "llm text"
    );
    assert_eq!(
        query_i64(&env, "SELECT epoch(mtime)::BIGINT FROM files").await,
        1_600_000_000,
        "stat data refreshed"
    );
}

#[tokio::test]
async fn changed_content_replaces_record() {
    let env = TestEnv::new();
    let file = env.root.join("a.png");
    write_png(&file, 32, 16);
    index(&env, json!({ "path": env.root })).await;
    let (id, sha) = (
        query_str(&env, "SELECT id FROM files").await,
        query_str(&env, "SELECT sha256 FROM files").await,
    );
    env.state
        .db
        .call(move |c| {
            c.execute_batch(
                "UPDATE files SET summary = 'old', summary_status = 'done';
                 INSERT INTO analyses (id, file_id, created_at) SELECT 'a1', id, now() FROM files;",
            )?;
            Ok(())
        })
        .await
        .unwrap();

    write_png(&file, 64, 48);
    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["processed"], 1, "{job}");
    assert_eq!(query_i64(&env, "SELECT count(*) FROM files").await, 1);
    assert_ne!(query_str(&env, "SELECT id FROM files").await, id);
    assert_ne!(query_str(&env, "SELECT sha256 FROM files").await, sha);
    assert_eq!(
        query_str(&env, "SELECT summary_status FROM files").await,
        "pending"
    );
    assert_eq!(query_i64(&env, "SELECT height FROM images").await, 48);
    assert_eq!(
        query_i64(&env, "SELECT count(*) FROM analyses").await,
        0,
        "old LLM history removed"
    );
}

#[tokio::test]
async fn removed_files_are_deleted_within_scope() {
    let env = TestEnv::new();
    populate(&env);
    index(&env, json!({ "path": env.root })).await;
    assert_eq!(query_i64(&env, "SELECT count(*) FROM files").await, 4);

    std::fs::remove_file(env.root.join("notes.txt")).unwrap();
    std::fs::remove_file(env.root.join("sub/b.png")).unwrap();

    // Non-recursive: only top-level records are reconciled; sub/b.png stays for now.
    let job = index(&env, json!({ "path": env.root, "traverse": false })).await;
    assert_eq!(job["removed"], 1, "{job}");
    assert_eq!(
        query_i64(
            &env,
            "SELECT count(*) FROM files WHERE rel_path = 'sub/b.png'"
        )
        .await,
        1
    );

    let job = index(&env, json!({ "path": env.root })).await;
    assert_eq!(job["removed"], 1, "{job}");
    assert_eq!(query_i64(&env, "SELECT count(*) FROM files").await, 2);
    let (_, r) = env
        .call(post_json(
            "/search",
            json!({ "q": { "text": "quick fox" } }),
        ))
        .await;
    assert_eq!(r["total"], 0, "deleted content is no longer searchable");
}

#[tokio::test]
async fn unreadable_directory_prevents_deletion() {
    use std::os::unix::fs::PermissionsExt;
    let env = TestEnv::new();
    populate(&env);
    index(&env, json!({ "path": env.root })).await;
    let sub = env.root.join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&sub).is_ok() {
        // running as root: permissions are not enforced
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let job = index(&env, json!({ "path": env.root })).await;
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(job["removed"], 0, "{job}");
    assert_eq!(
        query_i64(
            &env,
            "SELECT count(*) FROM files WHERE rel_path = 'sub/b.png'"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn identical_content_reuses_llm_results() {
    let env = TestEnv::new();
    write_png(&env.root.join("a.png"), 32, 16);
    index(&env, json!({ "path": env.root })).await;
    env.state
        .db
        .call(|c| {
            Ok(c.execute(
                "UPDATE files SET summary = 'a red square', summary_status = 'done'",
                [],
            )?)
        })
        .await
        .unwrap();

    std::fs::copy(env.root.join("a.png"), env.root.join("copy.png")).unwrap();
    index(&env, json!({ "path": env.root })).await;
    assert_eq!(
        query_i64(
            &env,
            "SELECT count(*) FROM files WHERE summary = 'a red square' AND summary_status = 'done'"
        )
        .await,
        2
    );
}
