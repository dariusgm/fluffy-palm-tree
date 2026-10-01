use std::path::Path;

use anyhow::Context;

use crate::db::models::ImageMeta;

/// Reads format and dimensions from the image header (does not decode pixels).
pub fn extract(path: &Path, mime: Option<&str>) -> anyhow::Result<ImageMeta> {
    let size = imagesize::size(path)
        .with_context(|| format!("reading image header of {}", path.display()))?;
    let format = mime
        .and_then(|m| m.strip_prefix("image/"))
        .map(str::to_string)
        .or_else(|| crate::detect::extension(path));
    Ok(ImageMeta {
        format,
        width: u32::try_from(size.width).ok(),
        height: u32::try_from(size.height).ok(),
    })
}
