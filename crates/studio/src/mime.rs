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
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("video/mp4")
    } else {
        None
    }
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
        assert_eq!(sniff_media_mime(b"not an image"), None);
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
