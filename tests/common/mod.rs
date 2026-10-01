#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, Response};
use http_body_util::BodyExt;
use media_search::config::Config;
use tower::ServiceExt;

pub fn test_config(root: &Path, data: &Path) -> Config {
    let raw = include_str!("../../config.example.toml")
        .replace("/mnt/share/example", &root.display().to_string())
        .replace("./data", &data.display().to_string());
    Config::from_toml(&raw).expect("test config")
}

pub async fn send(
    app: axum::Router,
    peer: &str,
    mut req: Request<Body>,
) -> (Response<Body>, serde_json::Value) {
    let peer: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.oneshot(req).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (Response::from_parts(parts, Body::empty()), json)
}

pub fn get(uri: &str) -> Request<Body> {
    Request::get(uri).body(Body::empty()).unwrap()
}

pub fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

pub struct TestEnv {
    pub tmp: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub state: media_search::AppState,
}

impl TestEnv {
    pub fn new() -> Self {
        Self::with_config(|_| {})
    }

    pub fn with_config(adjust: impl FnOnce(&mut Config)) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let mut cfg = test_config(&root, &tmp.path().join("data"));
        adjust(&mut cfg);
        let db = media_search::db::Db::open_in_memory().unwrap();
        let state = media_search::AppState::new(cfg, db).unwrap();
        Self { tmp, root, state }
    }

    pub fn app(&self) -> axum::Router {
        media_search::router(self.state.clone())
    }

    pub async fn call(&self, req: Request<Body>) -> (axum::http::StatusCode, serde_json::Value) {
        let (resp, body) = send(self.app(), "192.168.1.2:4000", req).await;
        (resp.status(), body)
    }

    /// Polls the job until it is no longer running.
    pub async fn wait_job(&self, id: &str) -> serde_json::Value {
        for _ in 0..600 {
            let (_, v) = self.call(get(&format!("/jobs/{id}"))).await;
            if v["status"] != "running" {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("job {id} did not finish");
    }

    pub fn staging_is_empty(&self) -> bool {
        let dir = &self.state.config.staging.dir;
        std::fs::read_dir(dir).unwrap().next().is_none()
    }
}

pub fn write_png(path: &Path, w: u32, h: u32) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]))
        .save(path)
        .unwrap();
}

/// Writes a minimal single-page PDF containing `text`.
pub fn write_pdf(path: &Path, text: &str) {
    let content = format!("BT /F1 24 Tf 72 720 Td ({text}) Tj ET");
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, obj) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{obj}\nendobj\n", i + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for o in offsets {
        out.push_str(&format!("{o:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    std::fs::write(path, out).unwrap();
}

/// Generates a short test video with ffmpeg; returns false if ffmpeg is unavailable.
pub fn write_video(path: &Path, secs: u32) -> bool {
    if !media_search::extract::tool_available("ffmpeg") {
        return false;
    }
    std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=duration={secs}:size=320x240:rate=25"))
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
