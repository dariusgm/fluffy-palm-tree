use std::io::Read;
use std::path::Path;

use crate::db::models::{ArchiveMeta, DocType, FileKind};

/// Bytes read from the start of a file for type detection.
pub const HEADER_LEN: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub kind: FileKind,
    pub mime: String,
    pub doc_type: Option<DocType>,
    /// Programming/markup language for `DocType::Code`.
    pub language: Option<String>,
    pub archive: Option<ArchiveMeta>,
}

pub fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

pub fn read_header(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(HEADER_LEN);
    std::fs::File::open(path)?
        .take(HEADER_LEN as u64)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// Classifies a file by its content (magic bytes), using the name only to refine
/// (e.g. `.tar.gz`, code language). Returns `None` for unsupported files.
pub fn classify(path: &Path, header: &[u8]) -> Option<Detected> {
    if header.is_empty() {
        return None;
    }
    let ext = extension(path);
    let mime = infer::get(header).map(|t| t.mime_type().to_string());

    if let Some(d) = classify_image_video(header, ext.as_deref(), mime.as_deref()) {
        return Some(d);
    }
    if infer::archive::is_pdf(header) {
        return Some(document("application/pdf", DocType::Pdf, None));
    }
    if let Some(archive) = classify_archive(path, header, mime.as_deref()) {
        return Some(Detected {
            kind: FileKind::Archive,
            mime: mime.unwrap_or_else(|| "application/octet-stream".into()),
            doc_type: None,
            language: None,
            archive: Some(archive),
        });
    }
    // Anything else with a recognised binary signature (office files, executables,
    // audio, databases, ...) is not supported (yet).
    if mime.as_deref().is_some_and(|m| !m.starts_with("text/")) {
        return None;
    }
    // UTF-16 text contains NUL bytes, so it is recognised by its byte-order mark.
    let utf16 = header.starts_with(b"\xff\xfe") || header.starts_with(b"\xfe\xff");
    if !utf16 && !looks_like_text(header) {
        return None;
    }
    let language = code_language(path, header);
    let (doc_type, default_mime) = match (&language, ext.as_deref()) {
        (_, Some("md" | "markdown")) => (DocType::Markdown, "text/markdown"),
        (Some(_), _) => (DocType::Code, "text/plain"),
        (None, _) => (DocType::Text, "text/plain"),
    };
    let mime = mime.unwrap_or_else(|| default_mime.to_string());
    Some(document(&mime, doc_type, language))
}

fn document(mime: &str, doc_type: DocType, language: Option<String>) -> Detected {
    Detected {
        kind: FileKind::Document,
        mime: mime.to_string(),
        doc_type: Some(doc_type),
        language,
        archive: None,
    }
}

fn classify_image_video(header: &[u8], ext: Option<&str>, mime: Option<&str>) -> Option<Detected> {
    let mime = mime?;
    // Formats the `image` crate can decode for the LLM; others (HEIC, RAW) are not supported yet.
    const IMAGES: &[&str] = &[
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "image/bmp",
        "image/tiff",
    ];
    let kind = if IMAGES.contains(&mime) {
        FileKind::Image
    } else if mime.starts_with("video/") || (infer::audio::is_ogg(header) && ext == Some("ogv")) {
        FileKind::Video
    } else {
        return None;
    };
    let mime = if mime == "audio/ogg" {
        "video/ogg"
    } else {
        mime
    };
    Some(Detected {
        kind,
        mime: mime.to_string(),
        doc_type: None,
        language: None,
        archive: None,
    })
}

fn classify_archive(path: &Path, header: &[u8], mime: Option<&str>) -> Option<ArchiveMeta> {
    use infer::archive as a;
    let compression = if a::is_gz(header) {
        "gzip"
    } else if a::is_bz2(header) {
        "bzip2"
    } else if a::is_xz(header) {
        "xz"
    } else if a::is_zst(header) {
        "zstd"
    } else if a::is_lz4(header) {
        "lz4"
    } else if a::is_lz(header) {
        "lzip"
    } else if a::is_z(header) {
        "compress"
    } else if a::is_zip(header) {
        // Office documents, EPUB, JAR, ... are zip containers with their own type;
        // only plain zip files are archives.
        return matches!(mime, None | Some("application/zip")).then(|| archive("zip", "zip"));
    } else if a::is_7z(header) {
        return Some(archive("7z", "7z"));
    } else if a::is_rar(header) {
        return Some(archive("rar", "rar"));
    } else if a::is_tar(header) {
        return Some(archive("none", "tar"));
    } else {
        return None;
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let tar_by_name = name.contains(".tar.")
        || [".tgz", ".tbz", ".tbz2", ".txz", ".tzst"]
            .iter()
            .any(|s| name.ends_with(s));
    let tar = tar_by_name || (compression == "gzip" && gzip_contains_tar(header));
    let suffix = match compression {
        "gzip" => "gz",
        "bzip2" => "bz2",
        "zstd" => "zst",
        "compress" => "Z",
        "lzip" => "lz",
        other => other,
    };
    let format = if tar {
        format!("tar.{suffix}")
    } else {
        suffix.to_string()
    };
    Some(archive(compression, &format))
}

fn archive(compression: &str, format: &str) -> ArchiveMeta {
    ArchiveMeta {
        compression: compression.to_string(),
        format: format.to_string(),
    }
}

/// Decompresses the start of a gzip stream and checks for the tar `ustar` signature.
fn gzip_contains_tar(header: &[u8]) -> bool {
    let mut buf = [0u8; 512];
    let mut decoder = flate2::read::GzDecoder::new(header);
    decoder.read_exact(&mut buf).is_ok() && infer::archive::is_tar(&buf)
}

/// Text heuristic: no NUL bytes, almost no control characters, and either valid UTF-8
/// (a multi-byte char may be cut at the end of the header) or plausible 8-bit legacy
/// text such as Latin-1 (few bytes >= 0x80).
fn looks_like_text(header: &[u8]) -> bool {
    if header.contains(&0) {
        return false;
    }
    let control = header
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    if control * 100 > header.len() {
        return false;
    }
    let utf8 = match std::str::from_utf8(header) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && header.len() - e.valid_up_to() < 4,
    };
    utf8 || header.iter().filter(|&&b| b >= 0x80).count() * 100 <= header.len() * 30
}

/// Language of a source/markup/config file, from its name or shebang line.
fn code_language(path: &Path, header: &[u8]) -> Option<String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let by_name = match name.as_str() {
        "dockerfile" | "containerfile" => Some("dockerfile"),
        "makefile" | "gnumakefile" => Some("makefile"),
        "cmakelists.txt" => Some("cmake"),
        _ => None,
    };
    if let Some(l) = by_name {
        return Some(l.to_string());
    }
    let by_ext = extension(path).and_then(|e| {
        Some(match e.as_str() {
            "py" | "pyw" | "pyi" => "python",
            "rs" => "rust",
            "js" | "mjs" | "cjs" | "jsx" => "javascript",
            "ts" | "tsx" | "mts" | "cts" => "typescript",
            "java" => "java",
            "kt" | "kts" => "kotlin",
            "scala" => "scala",
            "go" => "go",
            "c" | "h" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
            "cs" => "csharp",
            "swift" => "swift",
            "m" | "mm" => "objective-c",
            "rb" => "ruby",
            "php" => "php",
            "pl" | "pm" => "perl",
            "lua" => "lua",
            "r" => "r",
            "dart" => "dart",
            "ex" | "exs" => "elixir",
            "erl" => "erlang",
            "hs" => "haskell",
            "clj" | "cljs" => "clojure",
            "sh" | "bash" | "zsh" | "fish" | "ksh" => "shell",
            "ps1" | "psm1" => "powershell",
            "bat" | "cmd" => "batch",
            "sql" => "sql",
            "html" | "htm" | "xhtml" => "html",
            "css" => "css",
            "scss" | "sass" => "scss",
            "less" => "less",
            "vue" => "vue",
            "svelte" => "svelte",
            "xml" | "xsd" | "xsl" | "xslt" | "plist" | "manifest" => "xml",
            "svg" => "svg",
            "json" | "jsonc" | "geojson" | "gdoc" | "gsheet" | "gslides" | "ipynb" => "json",
            "yaml" | "yml" => "yaml",
            "toml" => "toml",
            "ini" | "cfg" | "conf" | "properties" => "ini",
            "gradle" => "gradle",
            "tex" => "latex",
            "proto" => "protobuf",
            "graphql" | "gql" => "graphql",
            "tf" => "terraform",
            "nix" => "nix",
            _ => return None,
        })
    });
    if let Some(l) = by_ext {
        return Some(l.to_string());
    }
    shebang_language(header).map(str::to_string)
}

fn shebang_language(header: &[u8]) -> Option<&'static str> {
    let first = header.split(|&b| b == b'\n').next()?;
    let line = std::str::from_utf8(first).ok()?.strip_prefix("#!")?;
    let interpreter = line
        .split_whitespace()
        .find(|w| !w.ends_with("/env") && !w.starts_with('-'))?
        .rsplit('/')
        .next()?;
    Some(match interpreter {
        i if i.starts_with("python") => "python",
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" => "shell",
        "node" | "nodejs" | "deno" | "bun" => "javascript",
        i if i.starts_with("ruby") => "ruby",
        i if i.starts_with("perl") => "perl",
        "php" => "php",
        "lua" => "lua",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0";

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn tar_block() -> Vec<u8> {
        let mut b = vec![0u8; 512];
        b[257..262].copy_from_slice(b"ustar");
        b
    }

    #[test]
    fn magic_bytes_win_over_extension() {
        let d = classify(Path::new("photo.jpg"), PNG).unwrap();
        assert_eq!((d.kind, d.mime.as_str()), (FileKind::Image, "image/png"));
        let d = classify(Path::new("no_extension"), JPEG).unwrap();
        assert_eq!(d.kind, FileKind::Image);
        let d = classify(Path::new("x.pdf"), b"%PDF-1.4\n").unwrap();
        assert_eq!(d.doc_type, Some(DocType::Pdf));
    }

    #[test]
    fn archives() {
        let a = |name: &str, data: &[u8]| classify(Path::new(name), data).unwrap().archive.unwrap();
        assert_eq!(a("x.gz", &gzip(b"hello")).format, "gz");
        assert_eq!(
            a("x.gz", &gzip(&tar_block())).format,
            "tar.gz",
            "tar detected inside gzip"
        );
        assert_eq!(a("backup.tar.gz", &gzip(b"hello")).format, "tar.gz");
        assert_eq!(a("x.zip", b"PK\x03\x04rest").compression, "zip");
        // An OpenDocument presentation is a zip container, but not an archive.
        let mut odp = b"PK\x03\x04".to_vec();
        odp.extend_from_slice(&[0u8; 26]);
        odp.extend_from_slice(b"mimetypeapplication/vnd.oasis.opendocument.presentation");
        assert!(classify(Path::new("talk.odp"), &odp).is_none());
        assert_eq!(a("x.7z", b"7z\xbc\xaf\x27\x1c\0\x04").format, "7z");
        let tar = a("x", &tar_block());
        assert_eq!(
            (tar.compression.as_str(), tar.format.as_str()),
            ("none", "tar")
        );
        let bz = a("x.tbz2", b"BZh91AY&SY");
        assert_eq!(
            (bz.compression.as_str(), bz.format.as_str()),
            ("bzip2", "tar.bz2")
        );
    }

    #[test]
    fn text_and_code() {
        let d = classify(Path::new("main.py"), b"import os\nprint(os.name)\n").unwrap();
        assert_eq!(
            (d.doc_type, d.language.as_deref()),
            (Some(DocType::Code), Some("python"))
        );
        let d = classify(Path::new("style.css"), b"body { color: red; }").unwrap();
        assert_eq!(d.language.as_deref(), Some("css"));
        let d = classify(
            Path::new("index.html"),
            b"<!DOCTYPE html><html><body>hi</body></html>",
        )
        .unwrap();
        assert_eq!(
            (d.language.as_deref(), d.mime.as_str()),
            (Some("html"), "text/html")
        );
        let d = classify(Path::new("deploy"), b"#!/usr/bin/env bash\necho hi\n").unwrap();
        assert_eq!(d.language.as_deref(), Some("shell"));
        let d = classify(Path::new("Dockerfile"), b"FROM ubuntu\n").unwrap();
        assert_eq!(d.language.as_deref(), Some("dockerfile"));
        let d = classify(Path::new("notes.txt"), "Grüße\n".as_bytes()).unwrap();
        assert_eq!((d.doc_type, d.language), (Some(DocType::Text), None));
        let d = classify(Path::new("README.md"), b"# Title\n").unwrap();
        assert_eq!(d.doc_type, Some(DocType::Markdown));
        let d = classify(Path::new("data.log"), b"plain log line\n").unwrap();
        assert_eq!(d.doc_type, Some(DocType::Text));
        // Latin-1 encoded German text is text as well
        let d = classify(
            Path::new("brief.txt"),
            b"Gr\xfc\xdfe aus K\xf6ln, viele Gr\xfc\xdfe",
        )
        .unwrap();
        assert_eq!(d.doc_type, Some(DocType::Text));
        let d = classify(Path::new("export.csv"), b"\xff\xfea\0;\0b\0").unwrap();
        assert_eq!(d.doc_type, Some(DocType::Text), "UTF-16 with BOM");
    }

    #[test]
    fn unsupported_binaries() {
        assert!(classify(Path::new("x.bin"), b"\x00\x01\x02\x03binary").is_none());
        assert!(classify(Path::new("x.exe"), b"MZ\x90\x00\x03\x00\x00\x00").is_none());
        assert!(classify(Path::new("empty.txt"), b"").is_none());
        // mostly high bytes without a signature: not text
        assert!(
            classify(
                Path::new("x.dat"),
                &[0xe3u8, 0x9f, 0xc8, 0xa1, 0xf0, 0x81, 0x92, 0xb7]
            )
            .is_none()
        );
    }

    #[test]
    fn utf8_cut_at_header_end_is_text() {
        let mut data = "a".repeat(HEADER_LEN - 1).into_bytes();
        data.push(0xc3); // first byte of a two-byte char, cut off
        assert!(looks_like_text(&data));
    }
}
