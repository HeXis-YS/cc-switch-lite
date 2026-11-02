pub(crate) fn is_json_mime(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        let mime = value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        mime == "application/json" || mime.ends_with("+json")
    })
}

pub(crate) fn is_sse_mime(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| value.contains("text/event-stream"))
}

#[cfg(test)]
mod tests {
    use super::{is_json_mime, is_sse_mime};

    #[test]
    fn classifies_json_mime_types() {
        assert!(!is_json_mime(None));
        assert!(!is_json_mime(Some("")));
        assert!(is_json_mime(Some("application/json")));
        assert!(is_json_mime(Some(" application/json ; charset=utf-8 ")));
        assert!(is_json_mime(Some("APPLICATION/JSON; CHARSET=UTF-8")));
        assert!(is_json_mime(Some("application/problem+json")));
        assert!(!is_json_mime(Some("application/jsonx")));
        assert!(!is_json_mime(Some("text/plain")));
    }

    #[test]
    fn classifies_sse_mime_types() {
        assert!(!is_sse_mime(None));
        assert!(!is_sse_mime(Some("")));
        assert!(is_sse_mime(Some("text/event-stream")));
        assert!(is_sse_mime(Some("text/event-stream; charset=utf-8")));
        assert!(!is_sse_mime(Some("application/json")));
    }
}
