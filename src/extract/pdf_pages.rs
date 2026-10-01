use std::path::{Path, PathBuf};
use std::time::Duration;

use super::run_tool;

/// Renders pages `1..=pages` of a PDF to JPEG files in `out_dir` (poppler's `pdftoppm`).
/// Returns `(page_number, path)` in page order.
pub async fn render(
    pdf: &Path,
    out_dir: &Path,
    pages: u32,
    dpi: u32,
) -> anyhow::Result<Vec<(u32, PathBuf)>> {
    let prefix = out_dir.join("page");
    let (dpi, last) = (dpi.to_string(), pages.max(1).to_string());
    run_tool(
        "pdftoppm",
        [
            "-r".as_ref(),
            dpi.as_ref(),
            "-jpeg".as_ref(),
            "-jpegopt".as_ref(),
            "quality=90".as_ref(),
            "-f".as_ref(),
            "1".as_ref(),
            "-l".as_ref(),
            last.as_ref(),
            pdf.as_os_str(),
            prefix.as_os_str(),
        ],
        Duration::from_secs(600),
    )
    .await?;

    // pdftoppm names files page-1.jpg or page-01.jpg depending on the page count.
    let mut out: Vec<(u32, PathBuf)> = std::fs::read_dir(out_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?;
            let n = stem.strip_prefix("page-")?.parse().ok()?;
            (p.extension()? == "jpg").then_some((n, p))
        })
        .collect();
    out.sort_by_key(|(n, _)| *n);
    anyhow::ensure!(!out.is_empty(), "pdftoppm rendered no pages");
    Ok(out)
}
