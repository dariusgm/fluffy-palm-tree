use std::path::Path;

use anyhow::Context;
use image::{DynamicImage, ImageDecoder, ImageReader, imageops::FilterType};

use crate::db::models::ImageMeta;

/// Images above this many pixels are not decoded for the perceptual hash.
const MAX_HASH_PIXELS: u64 = 120_000_000;

/// Reads format and dimensions from the image header, and computes the perceptual
/// hash (decoding the pixels; a failure there only leaves the hash empty).
pub fn extract(path: &Path, mime: Option<&str>) -> anyhow::Result<ImageMeta> {
    let size = imagesize::size(path)
        .with_context(|| format!("reading image header of {}", path.display()))?;
    let format = mime
        .and_then(|m| m.strip_prefix("image/"))
        .map(str::to_string)
        .or_else(|| crate::detect::extension(path));
    let phash = if (size.width as u64) * (size.height as u64) <= MAX_HASH_PIXELS {
        match dhash(path) {
            Ok(h) => Some(h),
            Err(e) => {
                tracing::debug!(path = %path.display(), "no perceptual hash: {e:#}");
                None
            }
        }
    } else {
        None
    };
    Ok(ImageMeta {
        format,
        width: u32::try_from(size.width).ok(),
        height: u32::try_from(size.height).ok(),
        phash,
    })
}

/// 64-bit difference hash: the (EXIF-oriented) image is reduced to 9x8 grey pixels and
/// each bit tells whether a pixel is brighter than its right neighbour. Similar pictures
/// (resized, re-encoded, near-identical shots) differ in only a few bits.
pub fn dhash(path: &Path) -> anyhow::Result<u64> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()
        .with_context(|| format!("unsupported image {}", path.display()))?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    let small = img.resize_exact(9, 8, FilterType::Triangle).to_luma8();
    let mut bits = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            bits = (bits << 1) | u64::from(small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0]);
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(path: &Path, w: u32, h: u32, reversed: bool) {
        image::RgbImage::from_fn(w, h, |x, y| {
            let v = (x * 255 / (w - 1)) as u8;
            let v = if reversed { 255 - v } else { v };
            image::Rgb([v, v.wrapping_add((y % 3) as u8), v])
        })
        .save(path)
        .unwrap();
    }

    #[test]
    fn hash_is_stable_across_sizes_and_differs_for_other_content() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            tmp.path().join("a.png"),
            tmp.path().join("b.png"),
            tmp.path().join("c.png"),
        );
        gradient(&a, 400, 300, false);
        gradient(&b, 100, 75, false);
        gradient(&c, 400, 300, true);
        let (ha, hb, hc) = (dhash(&a).unwrap(), dhash(&b).unwrap(), dhash(&c).unwrap());
        assert!((ha ^ hb).count_ones() <= 4, "{ha:x} vs {hb:x}");
        assert!((ha ^ hc).count_ones() >= 40, "{ha:x} vs {hc:x}");
    }

    #[test]
    fn undecodable_file_has_no_hash_but_keeps_metadata_error_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("broken.png");
        std::fs::write(&p, b"not an image").unwrap();
        assert!(dhash(&p).is_err());
        assert!(extract(&p, Some("image/png")).is_err());
    }
}
