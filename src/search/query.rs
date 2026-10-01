use chrono::{DateTime, Days, NaiveDate};
use duckdb::types::Value as DbValue;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::db::fts::BODY_SQL;

pub const DEFAULT_LIMIT: u32 = 20;
pub const MAX_LIMIT: u32 = 200;
const MAX_TERMS: usize = 32;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    #[serde(default)]
    pub q: Map<String, Value>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextMode {
    Fts,
    Substring,
}

impl TextMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TextMode::Fts => "fts",
            TextMode::Substring => "substring",
        }
    }
}

#[derive(Debug)]
pub struct BuiltQuery {
    pub sql: String,
    pub params: Vec<DbValue>,
    pub terms: Vec<String>,
    pub text_mode: Option<TextMode>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Clone, Copy)]
enum FieldType {
    /// Case-insensitive equality; accepts a scalar or a list (IN).
    Exact,
    /// Case-insensitive substring match.
    Contains,
    /// Number or `{gte, gt, lte, lt, eq}`.
    Number,
    /// ISO date/timestamp or range object.
    Time,
    /// Octal permission bits, e.g. "644" or "0755".
    Mode,
    Tag,
}

/// Search fields → SQL expressions. Column names only ever come from this table.
const FIELDS: &[(&str, FieldType, &str)] = &[
    ("id", FieldType::Exact, "f.id"),
    ("kind", FieldType::Exact, "f.kind"),
    ("root", FieldType::Exact, "f.root"),
    ("extension", FieldType::Exact, "f.extension"),
    ("mime", FieldType::Exact, "f.mime"),
    ("mode_str", FieldType::Exact, "f.mode_str"),
    ("summary_status", FieldType::Exact, "f.summary_status"),
    ("ocr_status", FieldType::Exact, "f.ocr_status"),
    ("sha256", FieldType::Exact, "f.sha256"),
    ("doc_type", FieldType::Exact, "d.doc_type"),
    ("format", FieldType::Exact, "i.format"),
    ("video_codec", FieldType::Exact, "v.video_codec"),
    ("audio_codec", FieldType::Exact, "v.audio_codec"),
    ("path", FieldType::Contains, "f.abs_path"),
    ("name", FieldType::Contains, "f.file_name"),
    ("summary", FieldType::Contains, "f.summary"),
    ("container", FieldType::Contains, "v.container"),
    ("width", FieldType::Number, "coalesce(i.width, v.width)"),
    ("height", FieldType::Number, "coalesce(i.height, v.height)"),
    ("duration_secs", FieldType::Number, "v.duration_secs"),
    ("fps", FieldType::Number, "v.fps"),
    ("size_bytes", FieldType::Number, "f.size_bytes"),
    ("page_count", FieldType::Number, "d.page_count"),
    ("uid", FieldType::Number, "f.uid"),
    ("gid", FieldType::Number, "f.gid"),
    ("mtime", FieldType::Time, "f.mtime"),
    ("modified", FieldType::Time, "f.mtime"),
    ("created", FieldType::Time, "f.created"),
    ("language", FieldType::Exact, "d.language"),
    ("encoding", FieldType::Exact, "d.encoding"),
    ("compression", FieldType::Exact, "ar.compression"),
    ("archive_format", FieldType::Exact, "ar.format"),
    ("indexed_at", FieldType::Time, "f.indexed_at"),
    ("mode", FieldType::Mode, "f.mode"),
    ("tag", FieldType::Tag, ""),
];

pub const SELECT_COLUMNS: &str = r#"f.id, f.root, f.rel_path, f.abs_path, f.file_name, f.extension,
    f.kind, f.mime, f.size_bytes, f.mode, f.mode_str, f.uid, f.gid, f.mtime, f.sha256,
    f.summary, f.summary_status, f.indexed_at,
    i.format, i.width, i.height,
    d.doc_type, d.page_count, left(d.content, 300),
    v.duration_secs, v.width, v.height, v.video_codec, v.audio_codec, v.fps, v.container, v.bitrate,
    f.created, d.language, d.encoding, ar.compression, ar.format, f.ocr_status,
    (SELECT left(string_agg(o.text, chr(10) ORDER BY o.page), 1000) FROM ocr o WHERE o.file_id = f.id)"#;

pub fn field_names() -> Vec<&'static str> {
    std::iter::once("text")
        .chain(FIELDS.iter().map(|(n, _, _)| *n))
        .collect()
}

pub fn build(req: &SearchRequest, fts_available: bool) -> Result<BuiltQuery, String> {
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = req.offset.unwrap_or(0);

    let mut join_params = Vec::new();
    let mut where_params = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    let mut terms = Vec::new();
    let mut text_mode = None;
    let mut score_join = String::new();
    let mut score_expr = "NULL::DOUBLE".to_string();

    for (key, value) in &req.q {
        if key == "text" {
            let text = scalar_string(key, value)?;
            terms = text
                .split_whitespace()
                .map(str::to_lowercase)
                .take(MAX_TERMS)
                .collect();
            if terms.is_empty() {
                return Err("q.text must not be empty".into());
            }
            if fts_available {
                text_mode = Some(TextMode::Fts);
                score_join = "JOIN (SELECT file_id, fts_main_search_docs.match_bm25(file_id, ?) AS score FROM search_docs) s ON s.file_id = f.id".into();
                join_params.push(DbValue::Text(text));
                score_expr = "s.score".into();
                conds.push("s.score IS NOT NULL".into());
            } else {
                text_mode = Some(TextMode::Substring);
                let parts =
                    vec![
                        format!("CASE WHEN {BODY_SQL} ILIKE ? ESCAPE '\\' THEN 1 ELSE 0 END");
                        terms.len()
                    ];
                score_expr = format!("CAST(({}) AS DOUBLE)", parts.join(" + "));
                // The score expression appears in SELECT and WHERE; the SELECT copy of
                // the parameters is added when the final parameter list is assembled.
                conds.push(format!("{score_expr} > 0"));
                where_params.extend(terms.iter().map(|t| DbValue::Text(like_pattern(t))));
            }
            continue;
        }

        let Some((_, ty, col)) = FIELDS.iter().find(|(n, _, _)| n == key) else {
            return Err(format!(
                "unknown search field {key:?}; allowed: {}",
                field_names().join(", ")
            ));
        };
        let (cond, params) = field_condition(key, *ty, col, value)?;
        conds.push(cond);
        where_params.extend(params);
    }

    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conds.join(" AND "))
    };
    let order = if text_mode.is_some() {
        "ORDER BY score DESC, f.abs_path"
    } else {
        "ORDER BY f.abs_path"
    };
    let sql = format!(
        r#"SELECT {SELECT_COLUMNS}, {score_expr} AS score, count(*) OVER () AS total
           FROM files f
           LEFT JOIN images i ON i.file_id = f.id
           LEFT JOIN documents d ON d.file_id = f.id
           LEFT JOIN videos v ON v.file_id = f.id
           LEFT JOIN archives ar ON ar.file_id = f.id
           {score_join}
           {where_sql}
           {order}
           LIMIT {limit} OFFSET {offset}"#
    );

    let mut params = join_params;
    // In substring mode the score expression in SELECT also carries placeholders,
    // and SELECT comes before WHERE.
    if text_mode == Some(TextMode::Substring) {
        params.extend(terms.iter().map(|t| DbValue::Text(like_pattern(t))));
    }
    params.extend(where_params);

    Ok(BuiltQuery {
        sql,
        params,
        terms,
        text_mode,
        limit,
        offset,
    })
}

fn field_condition(
    key: &str,
    ty: FieldType,
    col: &str,
    value: &Value,
) -> Result<(String, Vec<DbValue>), String> {
    Ok(match ty {
        FieldType::Exact => {
            let values = string_list(key, value)?;
            let marks = vec!["lower(?)"; values.len()].join(", ");
            (
                format!("lower({col}) IN ({marks})"),
                values.into_iter().map(DbValue::Text).collect(),
            )
        }
        FieldType::Contains => {
            let s = scalar_string(key, value)?;
            (
                format!("{col} ILIKE ? ESCAPE '\\'"),
                vec![DbValue::Text(like_pattern(&s))],
            )
        }
        FieldType::Number => numeric_condition(key, col, value)?,
        FieldType::Time => time_condition(key, col, value)?,
        FieldType::Mode => {
            let s = scalar_string(key, value)?;
            let digits = s.trim_start_matches("0o");
            let mode = u32::from_str_radix(digits, 8)
                .map_err(|_| format!("{key}: expected octal permissions like \"644\""))?;
            (format!("{col} = ?"), vec![DbValue::BigInt(mode.into())])
        }
        FieldType::Tag => {
            let values = string_list(key, value)?;
            let marks = vec!["lower(?)"; values.len()].join(", ");
            (
                format!(
                    "EXISTS (SELECT 1 FROM tags t WHERE t.file_id = f.id AND lower(t.tag) IN ({marks}))"
                ),
                values.into_iter().map(DbValue::Text).collect(),
            )
        }
    })
}

fn numeric_condition(
    key: &str,
    col: &str,
    value: &Value,
) -> Result<(String, Vec<DbValue>), String> {
    let ops = match value {
        Value::Object(map) => range_ops(key, map)?,
        other => vec![("=", other)],
    };
    let mut conds = Vec::new();
    let mut params = Vec::new();
    for (op, v) in ops {
        conds.push(format!("{col} {op} ?"));
        params.push(DbValue::Double(number(key, v)?));
    }
    Ok((conds.join(" AND "), params))
}

fn time_condition(key: &str, col: &str, value: &Value) -> Result<(String, Vec<DbValue>), String> {
    let ops = match value {
        Value::Object(map) => range_ops(key, map)?,
        Value::String(s) if NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() => {
            // A bare date matches that whole day.
            let day = NaiveDate::parse_from_str(s, "%Y-%m-%d").expect("checked");
            let next = day
                .checked_add_days(Days::new(1))
                .ok_or_else(|| format!("{key}: date out of range"))?;
            return Ok((
                format!("{col} >= CAST(? AS TIMESTAMP) AND {col} < CAST(? AS TIMESTAMP)"),
                vec![
                    DbValue::Text(day.to_string()),
                    DbValue::Text(next.to_string()),
                ],
            ));
        }
        other => vec![("=", other)],
    };
    let mut conds = Vec::new();
    let mut params = Vec::new();
    for (op, v) in ops {
        conds.push(format!("{col} {op} CAST(? AS TIMESTAMP)"));
        params.push(DbValue::Text(timestamp(key, v)?));
    }
    Ok((conds.join(" AND "), params))
}

fn range_ops<'a>(
    key: &str,
    map: &'a Map<String, Value>,
) -> Result<Vec<(&'static str, &'a Value)>, String> {
    if map.is_empty() {
        return Err(format!("{key}: empty range object"));
    }
    map.iter()
        .map(|(op, v)| {
            let sql_op = match op.as_str() {
                "eq" => "=",
                "gt" => ">",
                "gte" => ">=",
                "lt" => "<",
                "lte" => "<=",
                other => {
                    return Err(format!(
                        "{key}: unknown operator {other:?} (use eq, gt, gte, lt, lte)"
                    ));
                }
            };
            Ok((sql_op, v))
        })
        .collect()
}

fn scalar_string(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(format!("{key}: expected a string or number")),
    }
}

fn string_list(key: &str, value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::Array(items) if !items.is_empty() => {
            items.iter().map(|v| scalar_string(key, v)).collect()
        }
        Value::Array(_) => Err(format!("{key}: empty list")),
        other => Ok(vec![scalar_string(key, other)?]),
    }
}

fn number(key: &str, value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
    .ok_or_else(|| format!("{key}: expected a number"))
}

fn timestamp(key: &str, value: &Value) -> Result<String, String> {
    let s = scalar_string(key, value)?;
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
        return Ok(dt.naive_utc().to_string());
    }
    if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return Ok(d.to_string());
    }
    Err(format!(
        "{key}: expected an ISO date (2024-05-01) or RFC 3339 timestamp"
    ))
}

fn like_pattern(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(q: Value) -> SearchRequest {
        serde_json::from_value(json!({ "q": q })).unwrap()
    }

    #[test]
    fn height_as_string_or_number() {
        for v in [json!("300"), json!(300)] {
            let b = build(&req(json!({ "height": v })), true).unwrap();
            assert!(b.sql.contains("coalesce(i.height, v.height) = ?"));
            assert_eq!(b.params, vec![DbValue::Double(300.0)]);
            assert!(b.text_mode.is_none());
        }
    }

    #[test]
    fn ranges() {
        let b = build(
            &req(json!({ "height": { "gte": 1080, "lt": "2161" } })),
            true,
        )
        .unwrap();
        assert!(b.sql.contains(">= ?"));
        assert!(b.sql.contains("< ?"));
        assert_eq!(b.params.len(), 2);
        assert!(build(&req(json!({ "height": { "foo": 1 } })), true).is_err());
        assert!(build(&req(json!({ "height": "abc" })), true).is_err());
    }

    #[test]
    fn unknown_field_rejected() {
        let err = build(&req(json!({ "nope; DROP TABLE files": 1 })), true).unwrap_err();
        assert!(err.contains("unknown search field"));
    }

    #[test]
    fn exact_list_and_contains_escaping() {
        let b = build(
            &req(json!({ "kind": ["image", "video"], "path": "50%_off" })),
            true,
        )
        .unwrap();
        assert!(b.sql.contains("lower(f.kind) IN (lower(?), lower(?))"));
        assert!(b.params.contains(&DbValue::Text("%50\\%\\_off%".into())));
    }

    #[test]
    fn text_fts_and_fallback() {
        let b = build(&req(json!({ "text": "Red Jacket" })), true).unwrap();
        assert_eq!(b.text_mode, Some(TextMode::Fts));
        assert_eq!(b.terms, vec!["red", "jacket"]);
        assert_eq!(b.params, vec![DbValue::Text("Red Jacket".into())]);

        let b = build(
            &req(json!({ "text": "red jacket", "kind": "image" })),
            false,
        )
        .unwrap();
        assert_eq!(b.text_mode, Some(TextMode::Substring));
        // 2 terms in SELECT + 2 terms in WHERE + 1 kind
        assert_eq!(b.params.len(), 5);
        assert_eq!(b.sql.matches('?').count(), 5);
        assert!(build(&req(json!({ "text": "   " })), true).is_err());
    }

    #[test]
    fn mode_and_time() {
        let b = build(&req(json!({ "mode": "0644" })), true).unwrap();
        assert_eq!(b.params, vec![DbValue::BigInt(0o644)]);
        assert!(build(&req(json!({ "mode": "999" })), true).is_err());
        let b = build(&req(json!({ "mtime": "2024-05-01" })), true).unwrap();
        assert_eq!(b.params.len(), 2);
        let b = build(
            &req(json!({ "mtime": { "gte": "2024-05-01T10:00:00Z" } })),
            true,
        )
        .unwrap();
        assert_eq!(b.params, vec![DbValue::Text("2024-05-01 10:00:00".into())]);
        assert!(build(&req(json!({ "mtime": "yesterday" })), true).is_err());
    }

    #[test]
    fn limit_is_clamped() {
        let r: SearchRequest = serde_json::from_value(json!({ "q": {}, "limit": 10000 })).unwrap();
        assert_eq!(build(&r, true).unwrap().limit, MAX_LIMIT);
    }

    #[test]
    fn placeholder_count_matches_params() {
        for fts in [true, false] {
            let b = build(
                &req(json!({ "text": "x y", "kind": "image", "height": {"gte": 1}, "tag": ["a", "b"], "mtime": "2024-01-01" })),
                fts,
            )
            .unwrap();
            assert_eq!(b.sql.matches('?').count(), b.params.len(), "fts={fts}");
        }
    }
}
