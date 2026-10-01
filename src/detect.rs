use std::path::Path;

use crate::db::models::{DocType, FileKind};

const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];
const VIDEO_EXT: &[&str] = &["mp4", "mkv", "mov", "webm", "avi", "m4v", "ogv"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detected {
    pub kind: FileKind,
    pub doc_type: Option<DocType>,
}

pub fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Classifies a file by extension. Returns `None` for unsupported files.
pub fn detect(path: &Path) -> Option<Detected> {
    let ext = extension(path)?;
    let ext = ext.as_str();
    let (kind, doc_type) = if IMAGE_EXT.contains(&ext) {
        (FileKind::Image, None)
    } else if VIDEO_EXT.contains(&ext) {
        (FileKind::Video, None)
    } else {
        let doc = match ext {
            "txt" | "text" => DocType::Text,
            "md" | "markdown" => DocType::Markdown,
            "pdf" => DocType::Pdf,
            _ => return None,
        };
        (FileKind::Document, Some(doc))
    };
    Some(Detected { kind, doc_type })
}

/// MIME type from magic bytes, falling back to the extension.
pub fn sniff_mime(path: &Path, detected: Detected) -> Option<String> {
    if let Ok(Some(t)) = infer::get_from_path(path) {
        return Some(t.mime_type().to_string());
    }
    let ext = extension(path)?;
    let mime = match (detected.kind, detected.doc_type, ext.as_str()) {
        (_, Some(DocType::Text), _) => "text/plain",
        (_, Some(DocType::Markdown), _) => "text/markdown",
        (_, Some(DocType::Pdf), _) => "application/pdf",
        (FileKind::Image, _, "jpg" | "jpeg") => "image/jpeg",
        (FileKind::Image, _, "tif") => "image/tiff",
        (FileKind::Image, _, e) => return Some(format!("image/{e}")),
        (FileKind::Video, _, "mkv") => "video/x-matroska",
        (FileKind::Video, _, "mov") => "video/quicktime",
        (FileKind::Video, _, "avi") => "video/x-msvideo",
        (FileKind::Video, _, "ogv") => "video/ogg",
        (FileKind::Video, _, e) => return Some(format!("video/{e}")),
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_case_insensitive() {
        assert_eq!(detect(Path::new("a/B.JPG")).unwrap().kind, FileKind::Image);
        assert_eq!(detect(Path::new("v.mkv")).unwrap().kind, FileKind::Video);
        assert_eq!(detect(Path::new("v.OGV")).unwrap().kind, FileKind::Video);
        let d = detect(Path::new("x.md")).unwrap();
        assert_eq!(d.kind, FileKind::Document);
        assert_eq!(d.doc_type, Some(DocType::Markdown));
        assert_eq!(
            detect(Path::new("x.pdf")).unwrap().doc_type,
            Some(DocType::Pdf)
        );
        assert!(detect(Path::new("x.exe")).is_none());
        assert!(detect(Path::new("noext")).is_none());
    }
}
