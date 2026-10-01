mod common;

use std::io::Write;

use axum::http::StatusCode;
use common::{TestEnv, post_json, write_png};
use serde_json::{Value, json};

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn tar_with_one_file() -> Vec<u8> {
    let mut block = vec![0u8; 1024];
    block[..9].copy_from_slice(b"hello.txt");
    block[257..262].copy_from_slice(b"ustar");
    block
}

async fn index(env: &TestEnv) -> Value {
    let (status, resp) = env
        .call(post_json("/index", json!({ "path": env.root })))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{resp}");
    env.wait_job(resp["job_id"].as_str().unwrap()).await
}

async fn search(env: &TestEnv, q: Value) -> Vec<Value> {
    let (status, resp) = env.call(post_json("/search", json!({ "q": q }))).await;
    assert_eq!(status, StatusCode::OK, "{resp}");
    resp["results"].as_array().unwrap().clone()
}

fn names(results: &[Value]) -> Vec<&str> {
    results
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect()
}

fn populate(env: &TestEnv) {
    let r = &env.root;
    std::fs::write(
        r.join("app.py"),
        "import os\n\ndef greet():\n    print('hello world')\n",
    )
    .unwrap();
    std::fs::write(r.join("site.css"), "body { background: papayawhip; }\n").unwrap();
    std::fs::write(
        r.join("index.html"),
        "<!DOCTYPE html><html><body>Willkommen</body></html>",
    )
    .unwrap();
    std::fs::write(r.join("deploy"), "#!/bin/bash\necho deploying\n").unwrap();
    std::fs::write(r.join("brief.txt"), b"Gr\xfc\xdfe aus K\xf6ln").unwrap();
    std::fs::write(r.join("backup.gz"), gzip(&tar_with_one_file())).unwrap();
    std::fs::write(r.join("notes.gz"), gzip(b"just one compressed text")).unwrap();
    let mut zip = b"PK\x03\x04".to_vec();
    zip.extend_from_slice(&[0u8; 60]);
    std::fs::write(r.join("bundle.zip"), zip).unwrap();
    write_png(&r.join("mislabeled.jpg"), 4, 4);
    write_png(&r.join("no_extension"), 6, 6);
    std::fs::write(r.join("random.bin"), b"\x00\x9f\x12\x00\xff").unwrap();
}

#[tokio::test]
async fn classifies_by_content() {
    let env = TestEnv::new();
    populate(&env);
    let job = index(&env).await;
    assert_eq!(job["found"], 10, "{job}");
    assert_eq!(job["failed"], 0, "{job}");

    let code = search(&env, json!({ "doc_type": "code" })).await;
    let langs: Vec<(&str, &str)> = code
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap(),
                r["document"]["language"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        langs,
        vec![
            ("app.py", "python"),
            ("deploy", "shell"),
            ("index.html", "html"),
            ("site.css", "css")
        ]
    );
    assert_eq!(
        names(&search(&env, json!({ "language": "python" })).await),
        vec!["app.py"]
    );
    assert_eq!(
        names(&search(&env, json!({ "text": "papayawhip" })).await),
        vec!["site.css"]
    );

    let latin = search(&env, json!({ "encoding": "windows-1252" })).await;
    assert_eq!(names(&latin), vec!["brief.txt"]);
    assert_eq!(
        latin[0]["document"]["snippet"], "Grüße aus Köln",
        "decoded, not mangled"
    );
    assert_eq!(
        names(&search(&env, json!({ "text": "köln" })).await),
        vec!["brief.txt"]
    );

    let images = search(&env, json!({ "kind": "image" })).await;
    assert_eq!(names(&images), vec!["mislabeled.jpg", "no_extension"]);
    assert_eq!(
        images[0]["image"]["format"], "png",
        "magic bytes win over the extension"
    );
    assert_eq!(images[0]["summary_status"], "pending");
}

#[tokio::test]
async fn archives_have_compression_and_format() {
    let env = TestEnv::new();
    populate(&env);
    index(&env).await;
    let archives = search(&env, json!({ "kind": "archive" })).await;
    let found: Vec<(&str, &str, &str)> = archives
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap(),
                r["archive"]["compression"].as_str().unwrap(),
                r["archive"]["format"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            ("backup.gz", "gzip", "tar.gz"),
            ("bundle.zip", "zip", "zip"),
            ("notes.gz", "gzip", "gz"),
        ]
    );
    assert!(
        archives
            .iter()
            .all(|r| r["size_bytes"].as_i64().unwrap() > 0)
    );
    assert!(archives.iter().all(|r| r["summary_status"] == "skipped"));
    assert_eq!(
        names(&search(&env, json!({ "archive_format": "tar.gz" })).await),
        vec!["backup.gz"]
    );
    assert_eq!(
        search(&env, json!({ "compression": "gzip" })).await.len(),
        2
    );

    let (status, _) = env
        .call(post_json("/import_media", json!({ "kind": ["archive"] })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn created_and_modified_are_stored() {
    let env = TestEnv::new();
    std::fs::write(env.root.join("a.txt"), "hello").unwrap();
    index(&env).await;
    let r = &search(&env, json!({ "name": "a.txt" })).await[0];
    assert!(r["mtime"].is_string());
    // Local filesystems in CI/dev report birth time; some network mounts do not.
    if std::fs::metadata(env.root.join("a.txt"))
        .unwrap()
        .created()
        .is_ok()
    {
        assert!(r["created"].is_string(), "{r}");
        let today = r["created"].as_str().unwrap()[..10].to_string();
        assert_eq!(search(&env, json!({ "created": today })).await.len(), 1);
    }
    let day = r["mtime"].as_str().unwrap()[..10].to_string();
    assert_eq!(search(&env, json!({ "modified": day })).await.len(), 1);
}

#[tokio::test]
async fn old_documents_get_new_document_metadata() {
    let env = TestEnv::new();
    std::fs::write(env.root.join("script.py"), "print('hi')\n").unwrap();
    index(&env).await;
    env.state
        .db
        .call(|c| {
            c.execute_batch(
                "UPDATE files SET meta_version = 1;
                 UPDATE documents SET doc_type = 'text', language = NULL, encoding = NULL;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let id_before = search(&env, json!({ "name": "script.py" })).await[0]["id"].clone();

    let job = index(&env).await;
    assert_eq!(job["updated"], 1, "{job}");
    let r = &search(&env, json!({ "name": "script.py" })).await[0];
    assert_eq!(r["id"], id_before, "same record");
    assert_eq!(r["document"]["doc_type"], "code");
    assert_eq!(r["document"]["language"], "python");
    assert_eq!(r["document"]["encoding"], "ascii");
    assert!(env.staging_is_empty());
}

#[tokio::test]
async fn old_records_are_upgraded_without_losing_llm_results() {
    let env = TestEnv::new();
    write_png(&env.root.join("photo.png"), 8, 8);
    index(&env).await;
    env.state
        .db
        .call(|c| {
            c.execute_batch(
                "UPDATE files SET summary = 'a red square', summary_status = 'done',
                                  meta_version = 1, created = NULL;
                 INSERT INTO analyses (id, file_id, created_at) SELECT 'a1', id, now() FROM files;",
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let job = index(&env).await;
    assert_eq!(
        (
            job["updated"].as_u64(),
            job["processed"].as_u64(),
            job["removed"].as_u64()
        ),
        (Some(1), Some(0), Some(0)),
        "{job}"
    );
    let r = &search(&env, json!({ "name": "photo.png" })).await[0];
    assert_eq!(r["summary"], "a red square");
    assert_eq!(r["summary_status"], "done");
    let (meta_version, analyses): (i32, i64) = env
        .state
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT meta_version, (SELECT count(*) FROM analyses) FROM files",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        (meta_version, analyses),
        (media_search::db::models::META_VERSION, 1)
    );

    let job = index(&env).await;
    assert_eq!(
        (job["updated"].as_u64(), job["skipped"].as_u64()),
        (Some(0), Some(1)),
        "{job}"
    );
}
