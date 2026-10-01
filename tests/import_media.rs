mod common;

use axum::http::StatusCode;
use common::{TestEnv, get, post_json, write_png, write_video};
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ANSWER: &str = r#"{"summary": "A red square on a plain background.", "people": [{"apparent_age": "adult", "clothing": "green raincoat"}], "objects": ["square"], "scene": "studio", "visible_text": "", "tags": ["red", "minimal"]}"#;

fn completion(content: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }],
        "usage": { "prompt_tokens": 812, "completion_tokens": 64 }
    }))
}

async fn env_with_llm(server: &MockServer) -> TestEnv {
    let uri = server.uri();
    TestEnv::with_config(move |c| {
        c.llm.base_url = uri;
        c.llm.model = "test-model".into();
        c.llm.timeout_secs = 5;
    })
}

async fn run_job(env: &TestEnv, uri: &str, body: Value) -> Value {
    let (status, resp) = env.call(post_json(uri, body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    env.wait_job(resp["job_id"].as_str().unwrap()).await
}

async fn query(env: &TestEnv, sql: &'static str) -> Vec<Vec<String>> {
    env.state
        .db
        .call(move |c| {
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                let n = r.as_ref().column_count();
                (0..n)
                    .map(|i| r.get::<_, Option<String>>(i).map(|v| v.unwrap_or_default()))
                    .collect::<Result<Vec<_>, _>>()
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn imports_image_description_and_makes_it_searchable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("data:image/jpeg;base64,"))
        .and(body_string_contains("\"enable_thinking\":false"))
        .respond_with(completion(&format!("<think>looking</think>{ANSWER}")))
        .expect(1)
        .mount(&server)
        .await;

    let env = env_with_llm(&server).await;
    write_png(&env.root.join("square.png"), 64, 64);
    std::fs::write(env.root.join("notes.txt"), "not media").unwrap();
    run_job(&env, "/index", json!({ "path": env.root })).await;

    let job = run_job(&env, "/import_media", json!({})).await;
    assert_eq!(job["status"], "completed", "{job}");
    assert_eq!(
        (job["found"].as_u64(), job["processed"].as_u64()),
        (Some(1), Some(1)),
        "{job}"
    );
    assert!(env.staging_is_empty());

    let (_, r) = env
        .call(post_json(
            "/search",
            json!({ "q": { "text": "green raincoat" } }),
        ))
        .await;
    assert_eq!(r["results"][0]["name"], "square.png", "{r}");
    assert_eq!(r["results"][0]["summary_status"], "done");
    let summary = r["results"][0]["summary"].as_str().unwrap();
    assert!(summary.starts_with("A red square"), "{summary}");
    assert!(summary.contains("Tags: red, minimal."), "{summary}");

    let rows = query(
        &env,
        "SELECT model, prompt_version, CAST(prompt_tokens AS VARCHAR), error FROM analyses",
    )
    .await;
    let version = media_search::llm::prompts::IMAGE_PROMPT_VERSION;
    assert_eq!(rows, vec![vec!["test-model", version, "812", ""]]);

    // Nothing pending anymore: a second run finds nothing.
    let job = run_job(&env, "/import_media", json!({})).await;
    assert_eq!(job["found"], 0);
}

#[tokio::test]
async fn llm_errors_mark_items_failed_and_force_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("model not loaded"))
        .mount(&server)
        .await;

    let env = env_with_llm(&server).await;
    write_png(&env.root.join("a.png"), 16, 16);
    run_job(&env, "/index", json!({ "path": env.root })).await;

    let job = run_job(&env, "/import_media", json!({ "kind": ["image"] })).await;
    assert_eq!(job["failed"], 1, "{job}");
    assert!(
        job["recent_errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("500")
    );
    let rows = query(&env, "SELECT summary_status FROM files").await;
    assert_eq!(rows, vec![vec!["failed"]]);
    let errors = query(
        &env,
        "SELECT count(*)::VARCHAR FROM analyses WHERE error LIKE '%model not loaded%'",
    )
    .await;
    assert_eq!(errors[0][0], "1");

    // without force, failed items are not retried; with force they are
    let job = run_job(&env, "/import_media", json!({})).await;
    assert_eq!(job["found"], 0);
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(completion("just some prose, no json"))
        .mount(&server)
        .await;
    let job = run_job(&env, "/import_media", json!({ "force": true })).await;
    assert_eq!(job["processed"], 1, "{job}");
    let rows = query(&env, "SELECT summary, summary_status FROM files").await;
    assert_eq!(rows, vec![vec!["just some prose, no json", "done"]]);
}

#[tokio::test]
async fn filters_by_prefix_ids_and_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(completion(ANSWER))
        .mount(&server)
        .await;
    let env = env_with_llm(&server).await;
    write_png(&env.root.join("x/1.png"), 8, 8);
    write_png(&env.root.join("x/2.png"), 8, 8);
    write_png(&env.root.join("y/3.png"), 8, 8);
    run_job(&env, "/index", json!({ "path": env.root })).await;

    let prefix = env.root.join("x").display().to_string();
    let job = run_job(
        &env,
        "/import_media",
        json!({ "path_prefix": prefix, "limit": 1 }),
    )
    .await;
    assert_eq!(
        (job["found"].as_u64(), job["processed"].as_u64()),
        (Some(1), Some(1))
    );

    let id = query(&env, "SELECT id FROM files WHERE rel_path = 'y/3.png'").await[0][0].clone();
    let job = run_job(&env, "/import_media", json!({ "ids": [id] })).await;
    assert_eq!(job["processed"], 1);
    let pending = query(
        &env,
        "SELECT rel_path FROM files WHERE summary_status = 'pending'",
    )
    .await;
    assert_eq!(pending, vec![vec!["x/2.png"]]);
}

#[tokio::test]
async fn rejects_documents_and_unknown_fields() {
    let server = MockServer::start().await;
    let env = env_with_llm(&server).await;
    for body in [
        json!({ "kind": ["document"] }),
        json!({ "bogus": 1 }),
        json!({ "kind": ["audio"] }),
    ] {
        let (status, _) = env.call(post_json("/import_media", body.clone())).await;
        assert!(status.is_client_error(), "{body} -> {status}");
    }
}

#[tokio::test]
async fn llm_health() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": [{ "id": "test-model" }] })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "modalities": { "vision": false } })),
        )
        .mount(&server)
        .await;
    let env = env_with_llm(&server).await;
    let (status, body) = env.call(get("/health/llm")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reachable"], true);
    assert_eq!(body["configured_model_loaded"], true);
    assert_eq!(body["vision"], false);

    let down = TestEnv::with_config(|c| c.llm.base_url = "http://127.0.0.1:9".into());
    let (status, body) = down.call(get("/health/llm")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["reachable"], false);
}

async fn video_env(interval_secs: u32) -> Option<(MockServer, TestEnv)> {
    if !media_search::extract::tool_available("ffmpeg") {
        eprintln!("skipping: ffmpeg not installed");
        return None;
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("data:image/jpeg;base64,"))
        .respond_with(completion(
            r#"{"summary": "a test pattern with colored bars", "tags": ["bars"]}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("timestamps in seconds"))
        .respond_with(completion(
            r#"{"summary": "A synthetic test video showing color bars."}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let uri = server.uri();
    let env = TestEnv::with_config(move |c| {
        c.llm.base_url = uri;
        c.llm.timeout_secs = 5;
        c.llm.video_frame_interval_secs = interval_secs;
    });
    Some((server, env))
}

async fn import_video(env: &TestEnv, secs: u32) -> Vec<f64> {
    assert!(write_video(&env.root.join("clip.mp4"), secs));
    run_job(env, "/index", json!({ "path": env.root })).await;
    let job = run_job(env, "/import_media", json!({ "kind": ["video"] })).await;
    assert_eq!(job["processed"], 1, "{job}");
    assert!(env.staging_is_empty());
    query(
        env,
        "SELECT CAST(ts_secs AS VARCHAR) FROM video_frames ORDER BY ts_secs",
    )
    .await
    .into_iter()
    .map(|r| r[0].parse().unwrap())
    .collect()
}

#[tokio::test]
async fn video_samples_interval_plus_last_frame() {
    let Some((_server, env)) = video_env(10).await else {
        return;
    };
    // 0, 10, 20 periodic (fps=1/10 used to drop the 20 s sample) + last frame at 25
    let frames = import_video(&env, 25).await;
    assert_eq!(frames, vec![0.0, 10.0, 20.0, 25.0]);

    let (_, r) = env
        .call(post_json("/search", json!({ "q": { "text": "bars" } })))
        .await;
    assert_eq!(r["results"][0]["name"], "clip.mp4");
    assert_eq!(
        r["results"][0]["matched_frames"].as_array().unwrap().len(),
        4
    );
}

#[tokio::test]
async fn short_video_gets_first_and_last_frame() {
    let Some((_server, env)) = video_env(60).await else {
        return;
    };
    let frames = import_video(&env, 5).await;
    assert_eq!(frames, vec![0.0, 5.0]);
}
