use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use super::run_tool;
use crate::db::models::{DocType, DocumentMeta};

/// Upper bound for stored text per document.
pub const MAX_TEXT_BYTES: usize = 5 * 1024 * 1024;
const PDF_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn extract(
    path: &Path,
    doc_type: DocType,
    language: Option<String>,
) -> anyhow::Result<DocumentMeta> {
    match doc_type {
        DocType::Text | DocType::Markdown | DocType::Code => {
            let file = tokio::fs::File::open(path).await?;
            let mut buf = Vec::new();
            file.take(MAX_TEXT_BYTES as u64)
                .read_to_end(&mut buf)
                .await?;
            let (content, encoding) = decode_text(&buf);
            Ok(DocumentMeta {
                doc_type,
                page_count: None,
                content: Some(content),
                language,
                encoding: Some(encoding.to_string()),
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
                language: None,
                encoding: None,
            })
        }
    }
}

/// Decodes text and names its encoding: a byte-order mark (UTF-8/UTF-16) wins, then
/// UTF-8 (reported as `ascii` when there are no non-ASCII bytes), else Windows-1252,
/// the usual legacy encoding for Western European text (a superset of Latin-1).
pub fn decode_text(buf: &[u8]) -> (String, &'static str) {
    if let Some((enc, bom_len)) = encoding_rs::Encoding::for_bom(buf) {
        let (text, _) = enc.decode_without_bom_handling(&buf[bom_len..]);
        let name = match enc.name() {
            "UTF-16LE" => "utf-16le",
            "UTF-16BE" => "utf-16be",
            _ => "utf-8",
        };
        return (text.into_owned(), name);
    }
    // The buffer may end inside a multi-byte character because of MAX_TEXT_BYTES.
    let utf8_ok = match std::str::from_utf8(buf) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    if utf8_ok {
        let name = if buf.is_ascii() { "ascii" } else { "utf-8" };
        return (String::from_utf8_lossy(buf).into_owned(), name);
    }
    let (text, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(buf);
    (text.into_owned(), "windows-1252")
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
    fn decodes_and_names_encodings() {
        assert_eq!(decode_text(b"plain"), ("plain".to_string(), "ascii"));
        assert_eq!(
            decode_text("Grüße".as_bytes()),
            ("Grüße".to_string(), "utf-8")
        );
        assert_eq!(decode_text(b"\xef\xbb\xbfhi"), ("hi".to_string(), "utf-8"));
        assert_eq!(
            decode_text(b"Gr\xfc\xdfe \x80"),
            ("Grüße €".to_string(), "windows-1252")
        );
        assert_eq!(
            decode_text(b"\xff\xfeh\0i\0"),
            ("hi".to_string(), "utf-16le")
        );
        assert_eq!(
            decode_text(b"\xfe\xff\0h\0i"),
            ("hi".to_string(), "utf-16be")
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "ä".repeat(MAX_TEXT_BYTES);
        let t = truncate_utf8(s);
        assert!(t.len() <= MAX_TEXT_BYTES);
    }
}
