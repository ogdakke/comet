//! Attachment refs that ride prompt text (`withAttachments` transport):
//! plain local paths appended after a `Attached images (local files — …):`
//! trailer. The doc persists only that text, so the engine re-derives
//! `RunRequest.attachments` (absolute paths for inline image blocks) from a
//! queued prompt's stored text at delivery time.

/// The refs trailer marker, lowercased (the parse is case-insensitive,
/// matching the UI's tolerant `ATTACHED_IMAGES_RE`).
const MARKER: &str = "\n\nattached images (local files";

/// Paths listed in a prompt's attachment-refs trailer, in order. A prompt
/// without a trailer (or with a malformed one) has no refs.
pub fn attachment_refs(prompt: &str) -> Vec<String> {
    let lower = prompt.to_ascii_lowercase();
    let Some(rel) = lower.find(MARKER) else {
        return Vec::new();
    };
    let line_start = rel + MARKER.len();
    let rest = &prompt[line_start.min(prompt.len())..];
    // The marker line ends with `):`; everything after it is the ref list.
    let Some(marker_end) = rest.find('\n') else {
        return Vec::new();
    };
    if !rest[..marker_end].trim_end().ends_with("):") {
        return Vec::new();
    }
    rest[marker_end + 1..]
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .map(|line| line[2..].trim().to_string())
        .filter(|path| !path.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_refs_trailer() {
        let prompt = "look at these\n\nAttached images (local files — open them to view):\n- /tmp/a.png\n- /tmp/b.webp";
        assert_eq!(
            attachment_refs(prompt),
            vec!["/tmp/a.png".to_string(), "/tmp/b.webp".to_string()]
        );
    }

    #[test]
    fn no_trailer_means_no_refs() {
        assert!(attachment_refs("plain prompt").is_empty());
        assert!(attachment_refs("").is_empty());
    }

    #[test]
    fn malformed_marker_is_ignored() {
        assert!(
            attachment_refs(
                "hi\n\nAttached images (local files — open them to view): nothing\n- /a.png"
            )
            .is_empty()
        );
    }
}
