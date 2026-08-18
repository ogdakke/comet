//! Magic-byte sniffing for generated media.

/// Identify a persistable media type from file contents.
pub fn sniff_media_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        Some("image/gif")
    } else if let Some(mime) = sniff_iso_media(bytes) {
        Some(mime)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if is_mpeg_audio(bytes) {
        Some("audio/mpeg")
    } else {
        None
    }
}

fn sniff_iso_media(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let box_size = u32::from_be_bytes(bytes[0..4].try_into().ok()?) as usize;
    if box_size < 16 || box_size > bytes.len() {
        return None;
    }
    let major = &bytes[8..12];
    if is_still_image_brand(major) {
        return None;
    }
    if is_quicktime_brand(major) {
        return Some("video/quicktime");
    }
    if is_mp4_video_brand(major) {
        return Some("video/mp4");
    }
    let mut offset = 16;
    while offset + 4 <= box_size {
        let brand = &bytes[offset..offset + 4];
        if is_still_image_brand(brand) {
            offset += 4;
            continue;
        }
        if is_quicktime_brand(brand) {
            return Some("video/quicktime");
        }
        if is_mp4_video_brand(brand) {
            return Some("video/mp4");
        }
        offset += 4;
    }
    None
}

fn is_quicktime_brand(brand: &[u8]) -> bool {
    brand == b"qt  "
}

fn is_mp4_video_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"isom"
            | b"iso2"
            | b"iso3"
            | b"iso4"
            | b"iso5"
            | b"iso6"
            | b"mp41"
            | b"mp42"
            | b"avc1"
            | b"avc3"
            | b"dash"
    )
}

fn is_still_image_brand(brand: &[u8]) -> bool {
    matches!(
        brand,
        b"heic" | b"heix" | b"hevc" | b"hevx" | b"mif1" | b"msf1" | b"avif" | b"avis"
    )
}

fn is_mpeg_audio(bytes: &[u8]) -> bool {
    if bytes.len() >= 3 && bytes.starts_with(b"ID3") {
        return true;
    }
    if bytes.len() < 4 {
        return false;
    }
    bytes[0] == 0xff
        && bytes[1] & 0xe0 == 0xe0
        && bytes[1] & 0x18 != 0x08
        && bytes[1] & 0x06 != 0x00
        && bytes[2] >> 4 != 0x0f
        && (bytes[2] >> 2) & 0x03 != 0x03
}

/// Return the sniffed MIME when it is one of `accepted`.
pub fn accepted_output_mime(
    bytes: &[u8],
    accepted: impl IntoIterator<Item = impl AsRef<str>>,
) -> Option<String> {
    let sniffed = sniff_media_mime(bytes)?;
    accepted
        .into_iter()
        .any(|mime| mime.as_ref() == sniffed)
        .then(|| sniffed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{accepted_output_mime, sniff_media_mime};

    #[test]
    fn sniffs_common_image_headers() {
        assert_eq!(
            sniff_media_mime(b"\x89PNG\r\n\x1a\nrest"),
            Some("image/png")
        );
        assert_eq!(
            sniff_media_mime(&[0xff, 0xd8, 0xff, 0xe0]),
            Some("image/jpeg")
        );
        let mut webp = b"RIFF....WEBP".to_vec();
        webp[4..8].copy_from_slice(&[1, 0, 0, 0]);
        assert_eq!(sniff_media_mime(&webp), Some("image/webp"));
        let mut wav = b"RIFF....WAVE".to_vec();
        wav[4..8].copy_from_slice(&[1, 0, 0, 0]);
        assert_eq!(sniff_media_mime(&wav), Some("audio/wav"));
        assert_eq!(
            sniff_media_mime(&[0xff, 0xfb, 0x90, 0x00]),
            Some("audio/mpeg")
        );
        assert_eq!(sniff_media_mime(b"ID3\x04rest"), Some("audio/mpeg"));
        assert_eq!(sniff_media_mime(&ftyp(b"isom")), Some("video/mp4"));
        assert_eq!(sniff_media_mime(&ftyp(b"qt  ")), Some("video/quicktime"));
        assert_eq!(sniff_media_mime(&ftyp(b"heic")), None);
        assert_eq!(sniff_media_mime(&ftyp(b"avif")), None);
        assert_eq!(sniff_media_mime(b"not an image"), None);
    }

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::from(20u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(brand);
        bytes
    }

    #[test]
    fn accepted_mime_is_restricted_to_the_model_list() {
        let jpeg = [0xff, 0xd8, 0xff, 0xdb];
        assert_eq!(
            accepted_output_mime(&jpeg, ["image/jpeg", "image/png"]),
            Some("image/jpeg".into())
        );
        assert_eq!(
            accepted_output_mime(&jpeg, ["image/webp", "image/png"]),
            None
        );
    }
}
