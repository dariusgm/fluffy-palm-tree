mod common;

use axum::http::StatusCode;
use common::{TestEnv, post_json, write_pdf, write_png};
use media_search::config::OcrMode;
use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DESCRIBE: &str = "Describe this image";
const TRANSCRIBE: &str = "Transcribe all readable text";

fn answer(content: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "choices": [{ "message": { "content": content.to_string() }, "finish_reason": "stop" }]
    }))
}

async fn mock(server: &MockServer, marker: &str, content: Value, calls: u64) {
    Mock::given(method("POST"))
        .and(body_string_contains(marker))
        .respond_with(answer(content))
        .expect(calls)
        .mount(server)
        .await;
}

fn env_with(server: &MockServer, mode: OcrMode) -> TestEnv {
    let uri = server.uri();
    TestEnv::with_config(move |c| {
        c.llm.base_url = uri;
        c.llm.timeout_secs = 5;
        c.llm.ocr_images = mode;
    })
}

async fn run_job(env: &TestEnv, uri: &str, body: Value) -> Value {
    let (status, resp) = env.call(post_json(uri, body)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    env.wait_job(resp["job_id"].as_str().unwrap()).await
}

async fn find(env: &TestEnv, q: Value) -> Vec<Value> {
    let (_, r) = env.call(post_json("/search", json!({ "q": q }))).await;
    r["results"].as_array().unwrap().clone()
}

#[tokio::test]
async fn image_with_text_gets_description_and_ocr() {
    let server = MockServer::start().await;
    mock(
        &server,
        DESCRIBE,
        json!({ "summary": "A red street sign.", "visible_text": "STOP" }),
        1,
    )
    .await;
    mock(
        &server,
        TRANSCRIBE,
        json!({ "text": "STOP\nHauptstraße 42" }),
        1,
    )
    .await;
    let env = env_with(&server, OcrMode::Auto);
    write_png(&env.root.join("sign.png"), 32, 32);
    run_job(&env, "/index", json!({ "path": env.root })).await;

    let job = run_job(&env, "/import_media", json!({})).await;
    assert_eq!(
        (job["processed"].as_u64(), job["failed"].as_u64()),
        (Some(1), Some(0)),
        "{job}"
    );

    let hits = find(&env, json!({ "text": "hauptstraße" })).await;
    assert_eq!(hits.len(), 1, "OCR text is searchable");
    assert_eq!(hits[0]["ocr_status"], "done");
    assert_eq!(hits[0]["ocr_text"], "STOP\nHauptstraße 42");
    assert!(
        hits[0]["summary"]
            .as_str()
            .unwrap()
            .starts_with("A red street sign.")
    );
    assert_eq!(find(&env, json!({ "ocr_status": "done" })).await.len(), 1);

    // Both steps are done: nothing left to do.
    assert_eq!(run_job(&env, "/import_media", json!({})).await["found"], 0);
}

#[tokio::test]
async fn image_without_text_skips_the_ocr_call() {
    let server = MockServer::start().await;
    mock(
        &server,
        DESCRIBE,
        json!({ "summary": "A meadow.", "visible_text": "" }),
        1,
    )
    .await;
    mock(&server, TRANSCRIBE, json!({ "text": "" }), 0).await;
    let env = env_with(&server, OcrMode::Auto);
    write_png(&env.root.join("meadow.png"), 16, 16);
    run_job(&env, "/index", json!({ "path": env.root })).await;
    run_job(&env, "/import_media", json!({})).await;
    assert_eq!(
        find(&env, json!({ "name": "meadow" })).await[0]["ocr_status"],
        "none"
    );
}

#[tokio::test]
async fn already_described_image_only_gets_ocr() {
    let server = MockServer::start().await;
    mock(
        &server,
        DESCRIBE,
        json!({ "summary": "should not be called" }),
        0,
    )
    .await;
    mock(&server, TRANSCRIBE, json!({ "text": "EXIT" }), 1).await;
    let env = env_with(&server, OcrMode::Auto);
    write_png(&env.root.join("door.png"), 16, 16);
    run_job(&env, "/index", json!({ "path": env.root })).await;
    env.state
        .db
        .call(|c| {
            c.execute_batch(
                r#"UPDATE files SET summary = 'A green exit sign.', summary_status = 'done';
                   INSERT INTO analyses (id, file_id, prompt_version, parsed, created_at)
                   SELECT 'a1', id, 'img-v2', '{"visible_text": "EXIT"}', now() FROM files;"#,
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let job = run_job(&env, "/import_media", json!({})).await;
    assert_eq!(job["processed"], 1, "{job}");
    let hit = &find(&env, json!({ "name": "door" })).await[0];
    assert_eq!(
        hit["summary"], "A green exit sign.",
        "description untouched"
    );
    assert_eq!(hit["summary_status"], "done");
    assert_eq!(hit["ocr_text"], "EXIT");
}

#[tokio::test]
async fn ocr_never_mode_marks_images_skipped() {
    let server = MockServer::start().await;
    mock(
        &server,
        DESCRIBE,
        json!({ "summary": "A sign.", "visible_text": "STOP" }),
        1,
    )
    .await;
    mock(&server, TRANSCRIBE, json!({ "text": "STOP" }), 0).await;
    let env = env_with(&server, OcrMode::Never);
    write_png(&env.root.join("sign.png"), 16, 16);
    run_job(&env, "/index", json!({ "path": env.root })).await;
    run_job(&env, "/import_media", json!({})).await;
    assert_eq!(
        find(&env, json!({ "name": "sign" })).await[0]["ocr_status"],
        "skipped"
    );
}

#[tokio::test]
async fn scanned_pdf_is_described_and_transcribed() {
    if !media_search::extract::tool_available("pdftoppm") {
        eprintln!("skipping: poppler-utils not installed");
        return;
    }
    let server = MockServer::start().await;
    // Two PDFs are described (first page each); only the scan is transcribed.
    mock(
        &server,
        DESCRIBE,
        json!({ "summary": "A scanned letter on white paper." }),
        2,
    )
    .await;
    mock(
        &server,
        TRANSCRIBE,
        json!({ "text": "Sehr geehrte Damen und Herren,\nKündigung zum 31.12." }),
        1,
    )
    .await;
    let env = env_with(&server, OcrMode::Auto);
    write_pdf(&env.root.join("scan.pdf"), "");
    write_pdf(
        &env.root.join("text.pdf"),
        "Invoice Number 4711 for bicycle repair",
    );
    run_job(&env, "/index", json!({ "path": env.root })).await;

    let job = run_job(&env, "/import_media", json!({ "kind": ["document"] })).await;
    assert_eq!(
        (job["processed"].as_u64(), job["failed"].as_u64()),
        (Some(2), Some(0)),
        "{job}"
    );

    let scan = &find(&env, json!({ "text": "kündigung" })).await;
    assert_eq!(scan.len(), 1);
    assert_eq!(scan[0]["name"], "scan.pdf");
    assert_eq!(scan[0]["ocr_status"], "done");
    assert_eq!(scan[0]["summary_status"], "done");

    let text = &find(&env, json!({ "name": "text.pdf" })).await[0];
    assert_eq!(text["summary"], "A scanned letter on white paper.");
    assert!(text["ocr_status"].is_null(), "text layer present: no OCR");
    assert!(env.staging_is_empty());
}
