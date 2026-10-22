//! Credential boundary: preserve bytes unless an actual configured secret occurs.
//! JSON and SSE are scrubbed after parsing, so escaped strings remain valid JSON.
use axum::{
    body::{to_bytes, Body},
    http::header,
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;

pub fn scrub_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => *s = scrub_text(s, secrets),
        Value::Array(items) => {
            for item in items {
                scrub_value(item, secrets);
            }
        }
        Value::Object(object) => {
            let old = std::mem::take(object);
            for (k, mut v) in old {
                scrub_value(&mut v, secrets);
                object.insert(scrub_text(&k, secrets), v);
            }
        }
        _ => {}
    }
}
fn scrub_text(text: &str, secrets: &[String]) -> String {
    secrets
        .iter()
        .filter(|s| !s.is_empty())
        .fold(text.to_owned(), |text, key| text.replace(key, "[REDACTED]"))
}
fn scrub_document(raw: &[u8], secrets: &[String]) -> Vec<u8> {
    if let Ok(mut value) = serde_json::from_slice::<Value>(raw) {
        let old = value.clone();
        scrub_value(&mut value, secrets);
        if old != value {
            return serde_json::to_vec(&value).expect("JSON value serialization");
        }
        return raw.to_vec();
    }
    let mut output = raw.to_vec();
    for key in secrets.iter().filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        let mut offset = 0;
        while offset < output.len() {
            if output[offset..].starts_with(key.as_bytes()) {
                next.extend_from_slice(b"[REDACTED]");
                offset += key.len();
            } else {
                next.push(output[offset]);
                offset += 1;
            }
        }
        output = next;
    }
    output
}
fn scrub_event(raw: &str, secrets: &[String]) -> String {
    let mut data = Vec::new();
    for line in raw.lines() {
        if let Some(v) = crate::proxy::sse::strip_sse_field(line, "data") {
            data.push(v);
        }
    }
    let joined = data.join("\n");
    let scrubbed = scrub_document(joined.as_bytes(), secrets);
    if scrubbed == joined.as_bytes() {
        return scrub_text(raw, secrets);
    }
    let mut result = String::new();
    let mut wrote = false;
    for line in raw.lines() {
        if crate::proxy::sse::strip_sse_field(line, "data").is_some() {
            if !wrote {
                result.push_str("data: ");
                result.push_str(&String::from_utf8_lossy(&scrubbed));
                result.push('\n');
                wrote = true;
            }
        } else {
            result.push_str(&scrub_text(line, secrets));
            result.push('\n');
        }
    }
    result
}

fn next_event_boundary(buffer: &str) -> Option<(usize, usize)> {
    ["\r\n\r\n", "\n\n"]
        .into_iter()
        .filter_map(|separator| buffer.find(separator).map(|index| (index, separator.len())))
        .min_by_key(|(index, _)| *index)
}

pub async fn protect(response: Response, secrets: Vec<String>) -> Response {
    let (mut parts, body) = response.into_parts();
    for value in parts.headers.values_mut() {
        if let Ok(text) = value.to_str() {
            let scrubbed = scrub_text(text, &secrets);
            if scrubbed != text {
                *value = scrubbed.parse().expect("redacted header remains valid");
            }
        }
    }
    parts.headers.remove(header::CONTENT_LENGTH);
    let sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|s| s.contains("text/event-stream"));
    if sse {
        let stream = async_stream::stream! {
         let mut source=body.into_data_stream();let mut pending=String::new();let mut remainder=Vec::new();
         while let Some(chunk)=source.next().await {
          let chunk=match chunk {Ok(b)=>b,Err(_)=>{yield Err(std::io::Error::other("Upstream stream failed"));return;}};
          crate::proxy::sse::append_utf8_safe(&mut pending,&mut remainder,&chunk);
          while let Some((index, separator_len))=next_event_boundary(&pending) {
           let raw:String=pending.drain(..index+separator_len).collect();
           let protected=scrub_event(&raw,&secrets);
           let out=if protected==raw {raw}else{format!("{}\n\n",protected.trim_end_matches(['\r','\n']))};
           yield Ok::<_,std::io::Error>(Bytes::from(out));
          }
          if pending.len()>128*1024*1024 {yield Err(std::io::Error::other("SSE event exceeds response limit"));return;}
         }
         if !pending.is_empty(){yield Ok(Bytes::from(scrub_event(&pending,&secrets)));}
        };
        Response::from_parts(parts, Body::from_stream(stream))
    } else {
        match to_bytes(body, 128 * 1024 * 1024).await {
            Ok(bytes) => Response::from_parts(parts, Body::from(scrub_document(&bytes, &secrets))),
            Err(_) => {
                parts.status = axum::http::StatusCode::BAD_GATEWAY;
                parts.headers.remove(header::CONTENT_ENCODING);
                parts
                    .headers
                    .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
                Response::from_parts(
                    parts,
                    Body::from(
                        r#"{"error":{"type":"proxy_error","message":"Upstream body failed"}}"#,
                    ),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escaped_credentials_do_not_corrupt_json_or_ordinary_bytes() {
        let keys = vec!["key\"\\value".to_string()];
        let raw = br#"{ "text": "key\"\\value", "keep": 1 }"#;
        let output = scrub_document(raw, &keys);
        let parsed: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(parsed["text"], "[REDACTED]");
        let plain = b"{ \"text\": \"ordinary\" }";
        assert_eq!(scrub_document(plain, &keys), plain);
    }
    #[test]
    fn sse_unicode_escape_is_scrubbed_without_changing_unrelated_event() {
        let keys = vec!["secret".to_string()];
        let raw = "event: response.failed\ndata: {\"message\":\"secr\\u0065t\"}\n\n";
        let result = scrub_event(raw, &keys);
        assert!(result.contains("[REDACTED]"));
        let raw = "event: ping\r\ndata: {}\r\n\r\n";
        assert_eq!(scrub_event(raw, &keys), raw);
    }

    #[tokio::test]
    async fn protect_scrubs_unicode_escaped_secrets_across_mixed_sse_framing() {
        let secret = "secret";
        let raw = concat!(
            "event: response.failed\r\n",
            "data: {\"message\":\"secr\\u0065t\"}\r\n\r\n",
            "event: ping\n",
            "data: {}\n\n",
        );
        let mut response = Response::new(Body::from(raw));
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());

        let protected = protect(response, vec![secret.into()]).await;
        let bytes = to_bytes(protected.into_body(), 1024).await.unwrap();
        let output = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!output.contains("secr\\u0065t"));
        assert!(output.contains("data: {\"message\":\"[REDACTED]\"}\n\n"));
        assert!(output.contains("event: ping\ndata: {}\n\n"));
    }
}
