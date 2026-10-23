//! API-key response handling adapted from cc-switch 214e75ba handlers.rs and
//! response_processor.rs (MIT). Usage logging and connection accounting are omitted.
use super::{response_headers::*, response_helpers::*};
use crate::{
    config::Protocol,
    proxy::{
        content_encoding::{decompress_body_with_limit, get_content_encoding, DecompressError},
        error::ProxyError,
        providers::{
            codex_chat_history::{record_responses_sse_stream, CodexChatHistoryStore},
            responses_late_arguments::create_late_arguments_repair_stream,
            streaming_codex_anthropic::{
                create_responses_sse_stream_from_anthropic_with_context,
                responses_sse_events_from_anthropic_message,
            },
            streaming_codex_chat::create_responses_sse_stream_from_chat_with_context,
            transform_codex_anthropic,
            transform_codex_chat::{self, CodexToolContext},
            transform_codex_responses_namespace::{restore_response_namespaces, NamespacedName},
            transform_codex_responses_xai_sanitize::{
                create_xai_native_responses_sse_stream,
                normalize_xai_function_call_integer_arguments,
            },
        },
    },
};
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub(crate) struct ResponseOptions {
    pub model: String,
    pub streaming: bool,
    pub late_arguments_repair: bool,
    pub native_compaction: bool,
    pub xai_restore: Option<HashMap<String, NamespacedName>>,
}
const MAX_RESPONSE_BODY_BYTES: usize = 128 * 1024 * 1024;

/// Mirrors the upstream bounded body reader with failover disabled (no timeout).
pub async fn decode_response(
    response: reqwest::Response,
) -> Result<(HeaderMap, StatusCode, Bytes), ProxyError> {
    let mut headers = response.headers().clone();
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut raw = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            ProxyError::ForwardFailed(format!(
                "Failed to read response body: {}",
                crate::proxy::error::describe_reqwest_error(e)
            ))
        })?;
        if raw.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(ProxyError::ResponseBodyTooLarge(
                raw.len().saturating_add(chunk.len()),
            ));
        }
        raw.extend_from_slice(&chunk);
    }
    let mut body = Bytes::from(raw);
    if let Some(encoding) = get_content_encoding(&headers) {
        match decompress_body_with_limit(&encoding, &body, MAX_RESPONSE_BODY_BYTES) {
            Ok(Some(decoded)) => {
                body = Bytes::from(decoded);
                strip_entity_headers_for_rebuilt_body(&mut headers);
            }
            Ok(None) => {}
            Err(DecompressError::TooLarge { .. }) => {
                return Err(ProxyError::ResponseBodyTooLarge(MAX_RESPONSE_BODY_BYTES))
            }
            Err(DecompressError::Io(_)) => {}
        }
    }
    Ok((headers, status, body))
}
fn build(status: StatusCode, mut headers: HeaderMap, body: Body, rebuilt: bool) -> Response {
    strip_hop_by_hop_response_headers(&mut headers);
    if rebuilt {
        strip_entity_headers_for_rebuilt_body(&mut headers);
    }
    let mut result = Response::new(body);
    *result.status_mut() = status;
    *result.headers_mut() = headers;
    result
}
fn json_response(
    status: StatusCode,
    mut headers: HeaderMap,
    value: &Value,
) -> Result<Response, ProxyError> {
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    let bytes = serde_json::to_vec(value).map_err(|e| ProxyError::TransformError(e.to_string()))?;
    Ok(build(status, headers, Body::from(bytes), true))
}
fn sse_response(body: Body) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    build(StatusCode::OK, headers, body, true)
}

pub(crate) async fn handle(
    upstream: reqwest::Response,
    protocol: Protocol,
    options: ResponseOptions,
    context: CodexToolContext,
    history: Arc<CodexChatHistoryStore>,
) -> Result<Response, ProxyError> {
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let is_sse = content_type.is_some_and(|v| v.contains("text/event-stream"));
    let is_json = content_type.is_some_and(|v| {
        let mime = v
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        mime == "application/json" || mime.ends_with("+json")
    });
    if protocol == Protocol::Responses {
        if status.is_success() && is_sse {
            if options.native_compaction {
                return Ok(build(status, headers, Body::from_stream(crate::proxy::providers::codex_compaction::create_native_compaction_sse_stream(upstream.bytes_stream())),true));
            }
            if let Some(map) = options.xai_restore {
                return Ok(build(
                    status,
                    headers,
                    Body::from_stream(create_xai_native_responses_sse_stream(
                        upstream.bytes_stream(),
                        map,
                    )),
                    true,
                ));
            }
            if options.late_arguments_repair {
                return Ok(build(
                    status,
                    headers,
                    Body::from_stream(create_late_arguments_repair_stream(upstream.bytes_stream())),
                    true,
                ));
            }
        }
        if is_sse || (options.streaming && status.is_success() && content_type.is_none()) {
            return Ok(build(
                status,
                headers,
                Body::from_stream(upstream.bytes_stream()),
                false,
            ));
        }
        let (headers, status, bytes) = decode_response(upstream).await?;
        if status.is_success() {
            if let Some(map) = options.xai_restore {
                if let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
                    restore_response_namespaces(&mut value, &map);
                    normalize_xai_function_call_integer_arguments(&mut value);
                    return json_response(status, headers, &value);
                }
            }
        }
        return Ok(build(status, headers, Body::from(bytes), false));
    }
    if !status.is_success() {
        let (headers, status, bytes) = decode_response(upstream).await?;
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            let text = String::from_utf8_lossy(&bytes);
            let mut end = text.len().min(1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            Value::String(if end < text.len() {
                format!("{}…(truncated)", &text[..end])
            } else {
                text.into_owned()
            })
        });
        let value = transform_codex_chat::chat_error_to_response_error(Some(&parsed));
        if let Some(response) =
            codex_context_window_sse_response(&options.model, &value, status, options.streaming)
        {
            return Ok(response);
        }
        return json_response(status, headers, &value);
    }
    if (protocol == Protocol::ChatCompletions && (options.streaming || is_sse))
        || (protocol == Protocol::Messages && (is_sse || (options.streaming && !is_json)))
    {
        let source = upstream.bytes_stream();
        let body = if protocol == Protocol::ChatCompletions {
            Body::from_stream(record_responses_sse_stream(
                create_responses_sse_stream_from_chat_with_context(source, context),
                history,
            ))
        } else {
            Body::from_stream(create_responses_sse_stream_from_anthropic_with_context(
                source, context,
            ))
        };
        return Ok(sse_response(body));
    }
    let (headers, status, bytes) = decode_response(upstream).await?;
    let text = String::from_utf8_lossy(&bytes);
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if body_looks_like_sse(&text) => {
            if protocol == Protocol::ChatCompletions {
                chat_sse_to_response_value(&text)
                    .map_err(|e| aggregate_fallback_error(e, &headers, &text))?
            } else {
                transform_codex_anthropic::anthropic_sse_to_message_value(&text)?
            }
        }
        Err(e) => {
            return Err(upstream_body_parse_error(
                if protocol == Protocol::ChatCompletions {
                    "Failed to parse upstream chat response"
                } else {
                    "Failed to parse upstream anthropic response"
                },
                &e,
                &headers,
                &text,
            ))
        }
    };
    if protocol == Protocol::Messages && options.streaming {
        let events = responses_sse_events_from_anthropic_message(&value, context);
        return Ok(sse_response(Body::from_stream(futures::stream::iter(
            events.into_iter().map(Ok::<Bytes, std::io::Error>),
        ))));
    }
    let value = if protocol == Protocol::ChatCompletions {
        let value =
            transform_codex_chat::chat_completion_to_response_with_context(value, &context)?;
        history.record_response(&value).await;
        value
    } else {
        transform_codex_anthropic::anthropic_response_to_responses_with_context(value, &context)?
    };
    json_response(status, headers, &value)
}

mod error;
pub use error::{codex_context_window_sse_response, proxy_error_response};

#[cfg(test)]
mod tests;
