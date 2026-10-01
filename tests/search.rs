mod common;

use axum::http::StatusCode;
use common::{TestEnv, post_json, write_pdf, write_png};
use serde_json::{Value, json};

async fn indexed_env() -> TestEnv {
    let env = TestEnv::new();
    write_png(&env.root.join("a.png"), 32, 16);
    write_png(&env.root.join("vacation/b.png"), 8, 300);
    std::fs::write(env.root.join("notes.txt"), "the quick brown fox jumps").unwrap();
    std::fs::write(env.root.join("readme.md"), "# Holiday plans\nbeach and sun").unwrap();
    if media_search::extract::tool_available("pdftotext") {
        write_pdf(&env.root.join("invoice.pdf"), "Invoice Number 4711");
    }
    let (status, resp) = env
        .call(post_json("/index", json!({ "path": env.root })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let job = env.wait_job(resp["job_id"].as_str().unwrap()).await;
    assert_eq!(job["failed"], 0, "{job}");
    env
}

async fn search(env: &TestEnv, body: Value) -> Value {
    let (status, resp) = env.call(post_json("/search", body)).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    resp
}

fn names(resp: &Value) -> Vec<String> {
    resp["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn full_text_search() {
    let env = indexed_env().await;
    let r = search(&env, json!({ "q": { "text": "quick fox" } })).await;
    assert_eq!(r["text_mode"], "fts");
    assert_eq!(names(&r), vec!["notes.txt"]);
    assert_eq!(r["results"][0]["document"]["doc_type"], "text");
    assert!(r["results"][0]["score"].as_f64().unwrap() > 0.0);

    let r = search(&env, json!({ "q": { "text": "holiday" } })).await;
    assert_eq!(names(&r), vec!["readme.md"]);

    // file and directory names are searchable too
    let r = search(&env, json!({ "q": { "text": "vacation" } })).await;
    assert_eq!(names(&r), vec!["b.png"]);

    if media_search::extract::tool_available("pdftotext") {
        let r = search(&env, json!({ "q": { "text": "4711" } })).await;
        assert_eq!(names(&r), vec!["invoice.pdf"]);
        assert_eq!(r["results"][0]["document"]["page_count"], 1);
    }
}

#[tokio::test]
async fn metadata_search() {
    let env = indexed_env().await;
    let r = search(&env, json!({ "q": { "height": "300" } })).await;
    assert_eq!(names(&r), vec!["b.png"]);
    assert_eq!(r["total"], 1);
    assert_eq!(r["results"][0]["image"]["width"], 8);

    let r = search(
        &env,
        json!({ "q": { "kind": "image", "width": { "gte": 30 } } }),
    )
    .await;
    assert_eq!(names(&r), vec!["a.png"]);

    let r = search(
        &env,
        json!({ "q": { "kind": ["image", "document"], "path": "VACATION/" } }),
    )
    .await;
    assert_eq!(names(&r), vec!["b.png"]);

    let r = search(&env, json!({ "q": { "doc_type": "markdown" } })).await;
    assert_eq!(names(&r), vec!["readme.md"]);

    let mode = r["results"][0]["mode"].as_str().unwrap().to_string();
    let r = search(&env, json!({ "q": { "mode": mode, "name": "readme" } })).await;
    assert_eq!(names(&r), vec!["readme.md"]);

    let r = search(&env, json!({ "q": { "text": "fox", "kind": "image" } })).await;
    assert_eq!(r["total"], 0);
}

#[tokio::test]
async fn pagination_and_total() {
    let env = indexed_env().await;
    let r = search(&env, json!({ "q": { "kind": "image" }, "limit": 1 })).await;
    assert_eq!(r["total"], 2);
    assert_eq!(r["results"].as_array().unwrap().len(), 1);
    let r2 = search(
        &env,
        json!({ "q": { "kind": "image" }, "limit": 1, "offset": 1 }),
    )
    .await;
    assert_ne!(names(&r), names(&r2));
}

#[tokio::test]
async fn invalid_queries_are_rejected() {
    let env = indexed_env().await;
    for body in [
        json!({ "q": { "unknown": 1 } }),
        json!({ "q": { "height": "tall" } }),
        json!({ "q": { "height": { "between": 1 } } }),
        json!({ "q": { "text": "" } }),
        json!({ "q": { "mtime": "last week" } }),
        json!({ "query": {} }),
    ] {
        let (status, _) = env.call(post_json("/search", body.clone())).await;
        assert!(status.is_client_error(), "{body} -> {status}");
    }
}

#[tokio::test]
async fn substring_fallback_without_fts() {
    let env = indexed_env().await;
    env.state.db.set_fts_available(false);
    let r = search(
        &env,
        json!({ "q": { "text": "quick", "kind": "document" } }),
    )
    .await;
    assert_eq!(r["text_mode"], "substring");
    assert_eq!(names(&r), vec!["notes.txt"]);
}

#[tokio::test]
async fn summaries_and_video_frames_are_searchable() {
    let env = indexed_env().await;
    env.state
        .db
        .call(|c| {
            c.execute_batch(
                r#"UPDATE files SET summary = 'a woman with a red umbrella', summary_status = 'done'
                   WHERE file_name = 'a.png';
                   INSERT INTO files (id, root, rel_path, abs_path, file_name, kind, size_bytes,
                                      mode, mode_str, indexed_at)
                   VALUES ('v1', 'example', 'clip.mp4', '/x/clip.mp4', 'clip.mp4', 'video', 1,
                           420, 'rw-r--r--', now());
                   INSERT INTO videos (file_id, duration_secs, width, height, video_codec)
                   VALUES ('v1', 30, 1920, 1080, 'h264');
                   INSERT INTO video_frames VALUES ('v1', 0, 'empty street'),
                                                   ('v1', 10, 'a man riding a bicycle'),
                                                   ('v1', 20, 'bicycle parked at a wall');"#,
            )?;
            Ok(())
        })
        .await
        .unwrap();
    media_search::db::fts::rebuild(&env.state.db).await;

    let r = search(&env, json!({ "q": { "text": "umbrella" } })).await;
    assert_eq!(names(&r), vec!["a.png"]);

    let r = search(&env, json!({ "q": { "text": "bicycle" } })).await;
    assert_eq!(names(&r), vec!["clip.mp4"]);
    let frames = r["results"][0]["matched_frames"].as_array().unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["ts_secs"], 10.0);
    assert_eq!(r["results"][0]["video"]["height"], 1080);

    let r = search(
        &env,
        json!({ "q": { "kind": "video", "height": { "gte": 1080 }, "video_codec": "H264" } }),
    )
    .await;
    assert_eq!(names(&r), vec!["clip.mp4"]);
}
