use std::io::Cursor;
use std::path::Path;

use anyhow::Context;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageReader};

const JPEG_QUALITY: u8 = 85;

/// Decodes an image, applies its EXIF orientation, downscales it so the longest edge
/// is at most `max_edge`, and encodes it as JPEG for the LLM.
pub fn to_llm_jpeg(path: &Path, max_edge: u32) -> anyhow::Result<Vec<u8>> {
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()
        .with_context(|| format!("unsupported image {}", path.display()))?;
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);

    if img.width().max(img.height()) > max_edge {
        img = img.resize(max_edge, max_edge, image::imageops::FilterType::Triangle);
    }
    let rgb = img.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(Cursor::new(&mut out), JPEG_QUALITY).encode_image(&rgb)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscales_and_encodes_jpeg() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("big.png");
        image::RgbaImage::new(2000, 500).save(&p).unwrap();
        let jpeg = to_llm_jpeg(&p, 1024).unwrap();
        let img = image::load_from_memory(&jpeg).unwrap();
        assert_eq!((img.width(), img.height()), (1024, 256));
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }
}
