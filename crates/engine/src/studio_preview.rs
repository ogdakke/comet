//! Persistent gallery derivatives: a cover-capable JPEG and a thumbhash.
//!
//! Tiles never decode the original. These are written at ingest (and lazily
//! on first preview read) so scroll-back is a small file, not a 16-chunk RPC.
//! Videos use the same JPEG + thumbhash path: a first-frame poster, not the
//! bitstream.

use std::io::Cursor;

#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::DynamicImage;
use zeron_studio::sniff_media_mime;

/// Minimum short edge for a gallery preview. 512 covers a 248px tile at 2×
/// without keeping a 640px RGBA bitmap (~2.5MB) per cell in the GPU atlas.
pub const GALLERY_THUMB_SHORT_EDGE: u32 = 512;
/// Bound panoramas and unusually tall images without changing their aspect.
pub const GALLERY_THUMB_LONG_EDGE: u32 = 1536;

pub const PREVIEW_EXTENSION: &str = "jpg";
pub const PREVIEW_MIME: &str = "image/jpeg";

#[cfg(target_os = "macos")]
static POSTER_SEQ: AtomicU64 = AtomicU64::new(1);

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
    match sniff_media_mime(bytes) {
        Some(mime) if mime.starts_with("image/") => encode_still(bytes),
        Some(mime) if mime.starts_with("video/") => encode_video_poster(bytes),
        Some(mime) => Err(format!("cannot derive a gallery preview from {mime}")),
        None => encode_still(bytes).or_else(|_| encode_video_poster(bytes)),
    }
}

fn encode_still(bytes: &[u8]) -> Result<ArtifactPreview, String> {
    let image = image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    encode_image(image)
}

fn encode_video_poster(bytes: &[u8]) -> Result<ArtifactPreview, String> {
    let jpeg = poster_jpeg_from_video_bytes(bytes)
        .ok_or_else(|| "could not extract a video poster".to_string())?;
    encode_still(&jpeg)
}

fn encode_image(image: DynamicImage) -> Result<ArtifactPreview, String> {
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

/// First-frame JPEG for a gallery / feed poster. macOS uses AVFoundation;
/// other platforms return `None` and the tile keeps a play plate.
pub fn poster_jpeg_from_video_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let path = std::env::temp_dir().join(format!(
            "zeron-engine-poster-{}-{}.mp4",
            std::process::id(),
            POSTER_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).ok()?;
        let jpeg = macos::poster_jpeg(&path);
        let _ = std::fs::remove_file(&path);
        jpeg
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bytes;
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::path::Path;
    use std::ptr;

    use core_foundation::string::CFString;
    use objc::runtime::Object;
    use objc::{class, msg_send, sel, sel_impl};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CmTime {
        value: i64,
        timescale: i32,
        flags: u32,
        epoch: i64,
    }

    #[repr(C)]
    struct CgSize {
        width: f64,
        height: f64,
    }

    #[link(name = "AVFoundation", kind = "framework")]
    #[link(name = "CoreMedia", kind = "framework")]
    #[link(name = "ImageIO", kind = "framework")]
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CMTimeMake(value: i64, timescale: i32) -> CmTime;
        fn CGImageDestinationCreateWithData(
            data: core_foundation::data::CFMutableDataRef,
            type_: core_foundation::string::CFStringRef,
            count: usize,
            options: *const std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn CGImageDestinationAddImage(
            dest: *mut std::ffi::c_void,
            image: *mut std::ffi::c_void,
            properties: *const std::ffi::c_void,
        );
        fn CGImageDestinationFinalize(dest: *mut std::ffi::c_void) -> bool;
        fn CGImageRelease(image: *mut std::ffi::c_void);
    }

    pub(super) fn poster_jpeg(path: &Path) -> Option<Vec<u8>> {
        let path = path.to_str()?;
        let c_path = CString::new(path).ok()?;
        unsafe {
            let ns_path: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: c_path.as_ptr()];
            if ns_path.is_null() {
                return None;
            }
            let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: ns_path];
            if url.is_null() {
                return None;
            }
            let nil: *mut Object = ptr::null_mut();
            let asset: *mut Object =
                msg_send![class!(AVURLAsset), URLAssetWithURL: url options: nil];
            if asset.is_null() {
                return None;
            }
            let generator: *mut Object = msg_send![class!(AVAssetImageGenerator), alloc];
            let generator: *mut Object = msg_send![generator, initWithAsset: asset];
            if generator.is_null() {
                return None;
            }
            let _: () = msg_send![generator, setAppliesPreferredTrackTransform: true];
            let max = CgSize {
                width: 1280.0,
                height: 1280.0,
            };
            let _: () = msg_send![generator, setMaximumSize: max];
            let time = CMTimeMake(0, 600);
            let mut actual = CmTime {
                value: 0,
                timescale: 0,
                flags: 0,
                epoch: 0,
            };
            let mut error: *mut Object = ptr::null_mut();
            let image: *mut std::ffi::c_void = msg_send![
                generator,
                copyCGImageAtTime: time
                actualTime: &mut actual
                error: &mut error
            ];
            let _: () = msg_send![generator, release];
            if image.is_null() {
                return None;
            }
            let jpeg = jpeg_from_cgimage(image);
            CGImageRelease(image);
            jpeg
        }
    }

    unsafe fn jpeg_from_cgimage(image: *mut std::ffi::c_void) -> Option<Vec<u8>> {
        use core_foundation::base::{CFRelease, TCFType, kCFAllocatorDefault};
        use core_foundation::data::{CFDataCreateMutable, CFDataGetBytePtr, CFDataGetLength};
        unsafe {
            let data = CFDataCreateMutable(kCFAllocatorDefault, 0);
            if data.is_null() {
                return None;
            }
            let uti = CFString::new("public.jpeg");
            let dest =
                CGImageDestinationCreateWithData(data, uti.as_concrete_TypeRef(), 1, ptr::null());
            if dest.is_null() {
                CFRelease(data as _);
                return None;
            }
            CGImageDestinationAddImage(dest, image, ptr::null());
            let ok = CGImageDestinationFinalize(dest);
            CFRelease(dest as _);
            if !ok {
                CFRelease(data as _);
                return None;
            }
            let len = CFDataGetLength(data) as usize;
            let ptr = CFDataGetBytePtr(data);
            let bytes = if ptr.is_null() || len == 0 {
                None
            } else {
                Some(std::slice::from_raw_parts(ptr, len).to_vec())
            };
            CFRelease(data as _);
            bytes.filter(|bytes| !bytes.is_empty())
        }
    }
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

    fn ftyp_mp4() -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 20];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"isom");
        bytes
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

    #[test]
    fn header_only_mp4_does_not_derive_a_poster() {
        assert!(derive_preview(&ftyp_mp4()).is_err());
        assert!(poster_jpeg_from_video_bytes(&ftyp_mp4()).is_none());
        assert!(poster_jpeg_from_video_bytes(&[]).is_none());
    }

    #[test]
    fn audio_bytes_do_not_derive() {
        let mut wav = Vec::from(b"RIFF".as_slice());
        wav.extend_from_slice(&[0, 0, 0, 0]);
        wav.extend_from_slice(b"WAVE");
        assert!(derive_preview(&wav).is_err());
    }
}
