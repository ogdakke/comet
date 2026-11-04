//! Images a tool call put in front of the model, as harnesses report them,
//! turned into [`AgentEvent::ToolImage`]s for the engine to copy into uploads.

use zeron_proto::AgentEvent;

/// A tool image from bytes the harness carried (base64).
pub(crate) fn from_data(tool_id: &str, data: &str, mime_type: &str) -> AgentEvent {
    AgentEvent::ToolImage {
        tool_id: tool_id.to_owned(),
        path: String::new(),
        name: String::new(),
        mime_type: mime_type.to_owned(),
        data: Some(data.to_owned()),
    }
}

/// A tool image from a URL a harness gave: a base64 `data:` URL's bytes, or
/// the local file a `file://` URL or an absolute path names. `None` for
/// anything else (a remote URL is never fetched).
pub(crate) fn from_url(tool_id: &str, url: &str, mime_type: &str) -> Option<AgentEvent> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (head, data) = rest.split_once(',')?;
        let (mime, encoding) = head.rsplit_once(';')?;
        if !encoding.eq_ignore_ascii_case("base64") {
            return None;
        }
        let mime = if mime.is_empty() { mime_type } else { mime };
        return Some(from_data(tool_id, data, mime));
    }
    let path = url.strip_prefix("file://").map_or_else(|| url.to_owned(), percent_decode);
    std::path::Path::new(&path).is_absolute().then(|| {
        let name = path.rsplit(['/', '\\']).next().unwrap_or_default().to_owned();
        AgentEvent::ToolImage {
            tool_id: tool_id.to_owned(),
            path,
            name,
            mime_type: mime_type.to_owned(),
            data: None,
        }
    })
}

/// Whether a MIME type is one of the rasters a tool image may be.
pub(crate) fn is_raster_mime(mime_type: &str) -> bool {
    matches!(
        mime_type.to_ascii_lowercase().as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp"
    )
}

fn percent_decode(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = path.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_urls_carry_their_bytes_and_files_their_path() {
        let Some(AgentEvent::ToolImage { data, mime_type, path, .. }) =
            from_url("t", "data:image/png;base64,iVBORw0K", "")
        else {
            panic!("a data URL is an image");
        };
        assert_eq!((data.as_deref(), mime_type.as_str(), path.as_str()), (Some("iVBORw0K"), "image/png", ""));

        let Some(AgentEvent::ToolImage { data, path, name, .. }) =
            from_url("t", "file:///tmp/my%20shot.png", "image/png")
        else {
            panic!("a file URL is an image");
        };
        assert_eq!((data, path.as_str(), name.as_str()), (None, "/tmp/my shot.png", "my shot.png"));

        assert!(from_url("t", "https://example.com/a.png", "image/png").is_none());
        assert!(from_url("t", "data:image/png,notbase64", "image/png").is_none());
        assert!(from_url("t", "relative/a.png", "image/png").is_none());
    }
}
