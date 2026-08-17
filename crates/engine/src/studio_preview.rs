//! Persistent gallery derivatives: a cover-capable JPEG and a thumbhash.
//!
//! Tiles never decode the original. These are written at ingest (and lazily
//! on first preview read) so scroll-back is a small file, not a 16-chunk RPC.

use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::DynamicImage;

/// Minimum short edge for a gallery preview. 512 covers a 248px tile at 2×
/// without keeping a 640px RGBA bitmap (~2.5MB) per cell in the GPU atlas.
pub const GALLERY_THUMB_SHORT_EDGE: u32 = 512;
/// Bound panoramas and unusually tall images without changing their aspect.
pub const GALLERY_THUMB_LONG_EDGE: u32 = 1536;

pub const PREVIEW_EXTENSION: &str = "jpg";
pub const PREVIEW_MIME: &str = "image/jpeg";

pub fn preview_file_name(artifact_id: zeron_studio::StudioArtifactId) -> String {
    format!(
        "{}.{}.{}",
        artifact_id.0, GALLERY_THUMB_SHORT_EDGE, PREVIEW_EXTENSION
    )
}

pub struct ArtifactPreview {
    pub bytes: Vec<u8>,
    pub thumbhash: String,
}

pub fn gallery_thumb_dimensions(width: u32, height: u32) -> (u32, u32) {
    let short = width.min(height);
    let long = width.max(height);
    if short <= GALLERY_THUMB_SHORT_EDGE && long <= GALLERY_THUMB_LONG_EDGE {
        return (width, height);
    }
    let scale_for_short = GALLERY_THUMB_SHORT_EDGE as f64 / short.max(1) as f64;
    let scale_for_long = GALLERY_THUMB_LONG_EDGE as f64 / long.max(1) as f64;
    let scale = scale_for_short.min(scale_for_long).min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

pub fn derive_preview(bytes: &[u8]) -> Result<ArtifactPreview, String> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let (width, height) = gallery_thumb_dimensions(image.width(), image.height());
    let thumb = image.resize_exact(width, height, image::imageops::FilterType::Triangle);
    let mut encoded = Cursor::new(Vec::new());
    thumb
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .map_err(|error| error.to_string())?;
    Ok(ArtifactPreview {
        bytes: encoded.into_inner(),
        thumbhash: encode_thumbhash(&thumb)?,
    })
}

fn encode_thumbhash(image: &DynamicImage) -> Result<String, String> {
    // Official encoder requires each side ≤ 100.
    let small = image.thumbnail(100, 100);
    let rgba = small.to_rgba8();
    let hash =
        thumbhash::rgba_to_thumb_hash(rgba.width() as usize, rgba.height() as usize, rgba.as_raw());
    if hash.is_empty() {
        return Err("thumbhash encoder returned no bytes".into());
    }
    Ok(BASE64.encode(hash))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_png(width: u32, height: u32) -> Vec<u8> {
        let mut raw = Vec::new();
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([40, 120, 200]),
        ))
        .write_to(&mut Cursor::new(&mut raw), image::ImageFormat::Png)
        .unwrap();
        raw
    }

    #[test]
    fn preview_shrinks_a_large_frame() {
        let preview = derive_preview(&rgb_png(1200, 800)).unwrap();
        assert!(preview.bytes.len() < 1200 * 800);
        assert!(!preview.thumbhash.is_empty());
        let decoded = image::load_from_memory(&preview.bytes).unwrap();
        assert_eq!(decoded.width(), 768);
        assert_eq!(decoded.height(), GALLERY_THUMB_SHORT_EDGE);
    }

    #[test]
    fn preview_bounds_extreme_aspect_ratios() {
        assert_eq!(gallery_thumb_dimensions(4096, 1024), (1536, 384));
        assert_eq!(gallery_thumb_dimensions(800, 600), (683, 512));
        assert_eq!(gallery_thumb_dimensions(400, 300), (400, 300));
    }

    #[test]
    fn thumbhash_round_trips() {
        let preview = derive_preview(&rgb_png(64, 48)).unwrap();
        let hash = BASE64.decode(&preview.thumbhash).unwrap();
        let (width, height, rgba) = thumbhash::thumb_hash_to_rgba(&hash).unwrap();
        assert!(width > 0 && height > 0);
        assert_eq!(rgba.len(), width * height * 4);
    }

    #[test]
    fn garbage_bytes_do_not_derive() {
        assert!(derive_preview(b"\x89PNG\r\n\x1a\nnot-an-image").is_err());
    }
}
