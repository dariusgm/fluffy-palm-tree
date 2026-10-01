use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::run_tool;
use crate::db::models::{DocType, DocumentMeta};

/// Upper bound for stored text per document.
pub const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn extract(path: &Path, doc_type: DocType) -> anyhow::Result<DocumentMeta> {
    match doc_type {
        DocType::Text | DocType::Markdown => {
            let file = tokio::fs::File::open(path).await?;
            let mut buf = Vec::new();
            file.take(MAX_TEXT_BYTES as u64)
                .read_to_end(&mut buf)
                .await?;
            Ok(DocumentMeta {
                doc_type,
                page_count: None,
                content: Some(String::from_utf8_lossy(&buf).into_owned()),
            })
        }
        DocType::Pdf => {
            let text = run_tool(
                "pdftotext",
                [
                    "-layout".as_ref(),
                    "-enc".as_ref(),
                    "UTF-8".as_ref(),
                    path.as_os_str(),
                    "-".as_ref(),
                ],
                PDF_TIMEOUT,
            )
            .await?;
            let info = run_tool("pdfinfo", [path.as_os_str()], PDF_TIMEOUT).await;
            let page_count = info
                .ok()
                .and_then(|out| parse_pdfinfo_pages(&String::from_utf8_lossy(&out)));
            Ok(DocumentMeta {
                doc_type,
                page_count,
                content: Some(truncate_utf8(String::from_utf8_lossy(&text).into_owned())),
            })
        }
    }
}

fn parse_pdfinfo_pages(out: &str) -> Option<u32> {
    out.lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse().ok())
}

fn truncate_utf8(mut s: String) -> String {
    if s.len() > MAX_TEXT_BYTES {
        let mut cut = MAX_TEXT_BYTES;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_page_count() {
        let out = "Title: x\nPages:          12\nEncrypted: no\n";
        assert_eq!(parse_pdfinfo_pages(out), Some(12));
        assert_eq!(parse_pdfinfo_pages("nothing"), None);
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "ä".repeat(MAX_TEXT_BYTES);
        let t = truncate_utf8(s);
        assert!(t.len() <= MAX_TEXT_BYTES);
    }
}
