use std::collections::BTreeMap;
use std::path::{Component, Path};

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use duckdb::{OptionalExt, params};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::RootConfig;
use crate::db::models;
use crate::error::ApiError;
use crate::security::resolve_in_roots;
use crate::state::AppState;

const SAMPLE: usize = 20;
const MAX_IDS: usize = 10_000;

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRequest {
    /// File ids (`id` in search results).
    #[serde(default)]
    ids: Vec<String>,
    /// Absolute directory or file path inside a root; everything below it is selected.
    /// It does not have to exist any more. The root itself is refused.
    path_prefix: Option<String>,
    /// Also delete the files on the source share (never directories, except ones that
    /// become empty). Default: only the database records are removed.
    #[serde(default)]
    delete_source: bool,
    /// Only report what would happen. Default `true`; send `false` to act.
    #[serde(default = "yes")]
    dry_run: bool,
}

struct Matched {
    id: String,
    rel_path: String,
    abs_path: String,
    kind: String,
    size_bytes: i64,
}

pub async fn get_file(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, ApiError> {
    let row = state
        .db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT id, root, rel_path, abs_path, file_name, extension, kind, mime,
                        size_bytes, sha256, summary, summary_status, ocr_status
                 FROM files WHERE id = ?",
                params![id],
                |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "root": r.get::<_, String>(1)?,
                        "rel_path": r.get::<_, String>(2)?,
                        "abs_path": r.get::<_, String>(3)?,
                        "name": r.get::<_, String>(4)?,
                        "extension": r.get::<_, Option<String>>(5)?,
                        "kind": r.get::<_, String>(6)?,
                        "mime": r.get::<_, Option<String>>(7)?,
                        "size_bytes": r.get::<_, i64>(8)?,
                        "sha256": r.get::<_, Option<String>>(9)?,
                        "summary": r.get::<_, Option<String>>(10)?,
                        "summary_status": r.get::<_, Option<String>>(11)?,
                        "ocr_status": r.get::<_, Option<String>>(12)?,
                    }))
                },
            )
            .optional()?)
        })
        .await?;
    row.map(Json)
        .ok_or_else(|| ApiError::NotFound("file not found".into()))
}

/// Splits `requested` into (root name, path relative to that root).
fn split_prefix(roots: &[RootConfig], requested: &str) -> Result<(String, String), ApiError> {
    let path = Path::new(requested);
    if !path.is_absolute() {
        return Err(ApiError::BadRequest("path_prefix must be absolute".into()));
    }
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(ApiError::BadRequest("path_prefix must not contain '..'".into()));
    }
    for root in roots {
        let canonical = std::fs::canonicalize(&root.path).ok();
        for base in std::iter::once(root.path.clone()).chain(canonical) {
            if let Ok(rel) = path.strip_prefix(&base) {
                let rel = rel.to_str().unwrap_or_default().trim_end_matches('/');
                if rel.is_empty() {
                    return Err(ApiError::BadRequest(
                        "path_prefix is a whole root; select a sub-path".into(),
                    ));
                }
                return Ok((root.name.clone(), rel.to_string()));
            }
        }
    }
    Err(ApiError::Forbidden(
        "path_prefix is not inside a configured index root".into(),
    ))
}

pub async fn delete_files(
    State(state): State<AppState>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.ids.is_empty() && req.path_prefix.is_none() {
        return Err(ApiError::BadRequest(
            "give `ids` and/or `path_prefix`; nothing is selected by default".into(),
        ));
    }
    if req.ids.len() > MAX_IDS {
        return Err(ApiError::BadRequest(format!("at most {MAX_IDS} ids")));
    }
    let prefix = req
        .path_prefix
        .as_deref()
        .map(|p| split_prefix(&state.config.roots, p))
        .transpose()?;

    let ids = req.ids.clone();
    let matched: Vec<Matched> = state
        .db
        .call(move |c| {
            let mut found: BTreeMap<String, Matched> = BTreeMap::new();
            let cols = "SELECT id, rel_path, abs_path, kind, size_bytes FROM files";
            let map = |r: &duckdb::Row<'_>| -> duckdb::Result<Matched> {
                Ok(Matched {
                    id: r.get(0)?,
                    rel_path: r.get(1)?,
                    abs_path: r.get(2)?,
                    kind: r.get(3)?,
                    size_bytes: r.get(4)?,
                })
            };
            for id in &ids {
                let mut stmt = c.prepare(&format!("{cols} WHERE id = ?"))?;
                for m in stmt.query_map(params![id], map)? {
                    let m = m?;
                    found.insert(m.id.clone(), m);
                }
            }
            if let Some((root, rel)) = prefix {
                let mut stmt = c.prepare(&format!(
                    "{cols} WHERE root = ? AND (rel_path = ? OR starts_with(rel_path, ?))"
                ))?;
                for m in stmt.query_map(params![root, rel, format!("{rel}/")], map)? {
                    let m = m?;
                    found.insert(m.id.clone(), m);
                }
            }
            Ok(found.into_values().collect())
        })
        .await?;

    let mut by_kind: BTreeMap<&str, u64> = BTreeMap::new();
    for m in &matched {
        *by_kind.entry(m.kind.as_str()).or_default() += 1;
    }
    let bytes: i64 = matched.iter().map(|m| m.size_bytes).sum();
    let sample: Vec<&str> = matched.iter().take(SAMPLE).map(|m| m.rel_path.as_str()).collect();
    let mut resp = json!({
        "dry_run": req.dry_run,
        "delete_source": req.delete_source,
        "matched": matched.len(),
        "bytes": bytes,
        "by_kind": by_kind,
        "sample": sample,
    });
    if req.dry_run {
        return Ok(Json(resp));
    }

    // Source files first; a record is only dropped once its file is gone (or was already).
    let mut errors: Vec<Value> = Vec::new();
    let (mut deleted_source, mut already_missing) = (0u64, 0u64);
    let mut drop_ids: Vec<String> = Vec::with_capacity(matched.len());
    if req.delete_source {
        let roots = state.config.roots.clone();
        let work: Vec<(String, String, String)> = matched
            .iter()
            .map(|m| (m.id.clone(), m.rel_path.clone(), m.abs_path.clone()))
            .collect();
        let results = tokio::task::spawn_blocking(move || {
            work.into_iter()
                .map(|(id, rel, abs)| (id, rel, remove_source_file(&roots, &abs)))
                .collect::<Vec<_>>()
        })
        .await
        .map_err(anyhow::Error::from)?;
        for (id, rel, result) in results {
            match result {
                Ok(true) => {
                    deleted_source += 1;
                    drop_ids.push(id);
                }
                Ok(false) => {
                    already_missing += 1;
                    drop_ids.push(id);
                }
                Err(e) => errors.push(json!({ "path": rel, "error": e })),
            }
        }
    } else {
        drop_ids = matched.iter().map(|m| m.id.clone()).collect();
    }

    let removed = state
        .db
        .call(move |c| {
            for id in &drop_ids {
                models::delete_file(c, id)?;
            }
            Ok(drop_ids.len())
        })
        .await?;
    tracing::info!(removed, deleted_source, "deleted files via API");

    resp["removed_records"] = json!(removed);
    if req.delete_source {
        resp["deleted_source_files"] = json!(deleted_source);
        resp["already_missing"] = json!(already_missing);
    }
    resp["error_count"] = json!(errors.len());
    errors.truncate(SAMPLE);
    resp["errors"] = json!(errors);
    Ok(Json(resp))
}

/// Deletes one regular file inside a root, then prunes directories that became empty
/// (never the root). Ok(false) means the file was already gone.
fn remove_source_file(roots: &[RootConfig], abs: &str) -> Result<bool, String> {
    let path = Path::new(abs);
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() {
        return Err("not a regular file (symlink or directory), left untouched".into());
    }
    let (parent, name) = path
        .parent()
        .zip(path.file_name())
        .ok_or("path has no parent")?;
    let resolved = resolve_in_roots(roots, parent).map_err(|e| e.to_string())?;
    std::fs::remove_file(resolved.path.join(name)).map_err(|e| e.to_string())?;

    let mut dir = resolved.path.as_path();
    while dir != resolved.root_path && dir.starts_with(&resolved.root_path) {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    Ok(true)
}
