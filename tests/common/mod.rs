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
