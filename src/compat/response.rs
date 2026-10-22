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
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
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

pub fn codex_context_window_sse_response(
    model: &str,
    body: &Value,
    status: StatusCode,
    is_stream: bool,
) -> Option<axum::response::Response> {
    if !is_stream
        || status != StatusCode::BAD_REQUEST
        || body.pointer("/error/code").and_then(Value::as_str) != Some("context_length_exceeded")
    {
        return None;
    }
    let response = json!({
        "id": format!("resp_{}", uuid::Uuid::new_v4().simple()),
        "object": "response",
        "created_at": chrono::Utc::now().timestamp(),
        "status": "failed",
        "model": model,
        "output": [],
        "error": body["error"],
    });
    Some(
        (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "text/event-stream"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            crate::proxy::providers::codex_responses_sse::response_failed(&response),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(
        status: StatusCode,
        kind: Option<&str>,
        body: impl Into<Bytes>,
    ) -> reqwest::Response {
        let mut builder = axum::http::Response::builder().status(status);
        if let Some(kind) = kind {
            builder = builder.header(header::CONTENT_TYPE, kind);
        }
        builder.body(body.into()).unwrap().into()
    }
    fn options(streaming: bool) -> ResponseOptions {
        ResponseOptions {
            model: "test".into(),
            streaming,
            late_arguments_repair: false,
            native_compaction: false,
            xai_restore: None,
        }
    }
    async fn run(response: reqwest::Response, protocol: Protocol, streaming: bool) -> Response {
        handle(
            response,
            protocol,
            options(streaming),
            Default::default(),
            Arc::default(),
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn unmarked_chat_sse_is_aggregated_for_nonstreaming_client() {
        let source="data: {\"id\":\"c1\",\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n";
        let result = run(
            response(StatusCode::OK, None, source),
            Protocol::ChatCompletions,
            false,
        )
        .await;
        assert_eq!(result.headers()[header::CONTENT_TYPE], "application/json");
        let body = axum::body::to_bytes(result.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["output"][0]["content"][0]["text"], "hello");
    }
    #[tokio::test]
    async fn context_error_uses_failed_sse_only_for_streaming_bad_request() {
        let value = json!({"error":{"code":"context_length_exceeded","type":"invalid_request_error","message":"context too long"}});
        for (streaming, status, expected) in [
            (true, StatusCode::BAD_REQUEST, StatusCode::OK),
            (false, StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST),
            (
                true,
                StatusCode::TOO_MANY_REQUESTS,
                StatusCode::TOO_MANY_REQUESTS,
            ),
        ] {
            let result = run(
                response(status, Some("application/json"), value.to_string()),
                Protocol::ChatCompletions,
                streaming,
            )
            .await;
            assert_eq!(result.status(), expected);
            let body = axum::body::to_bytes(result.into_body(), usize::MAX)
                .await
                .unwrap();
            if expected == StatusCode::OK {
                assert!(String::from_utf8_lossy(&body).contains("response.failed"));
            }
        }
    }
    #[tokio::test]
    async fn non_json_error_retains_diagnostic_and_status() {
        let result = run(
            response(
                StatusCode::BAD_GATEWAY,
                Some("text/plain"),
                "upstream unavailable",
            ),
            Protocol::Messages,
            false,
        )
        .await;
        assert_eq!(result.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(result.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("upstream unavailable"));
    }
    #[tokio::test]
    async fn anthropic_explicit_json_streaming_response_synthesizes_sse() {
        let value = json!({"id":"msg_1","type":"message","role":"assistant","model":"test","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}});
        let result = run(
            response(
                StatusCode::OK,
                Some("Application/JSON; charset=utf-8"),
                value.to_string(),
            ),
            Protocol::Messages,
            true,
        )
        .await;
        let body = axum::body::to_bytes(result.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("response.completed"));
        assert!(text.contains("hello"));
    }
    #[tokio::test]
    async fn gzip_body_is_decoded_and_entity_headers_removed() {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"{\"ok\":true}").unwrap();
        let body = encoder.finish().unwrap();
        let response = axum::http::Response::builder()
            .header("content-encoding", "gzip")
            .header("content-length", body.len())
            .body(Bytes::from(body))
            .unwrap()
            .into();
        let (headers, _, body) = decode_response(response).await.unwrap();
        assert_eq!(&body[..], b"{\"ok\":true}");
        assert!(!headers.contains_key("content-encoding"));
        assert!(!headers.contains_key("content-length"));
    }
}

#[path = "response_error_mapper.rs"]
mod error_mapper;
use error_mapper::{get_error_message, map_proxy_error_to_status};
fn codex_proxy_error_json(
    provider_name: &str,
    request_model: &str,
    endpoint: &str,
    error: &ProxyError,
) -> Value {
    let (mut body, upstream_status) = match error {
        ProxyError::UpstreamError { status, body } => {
            let parsed_body = body
                .as_deref()
                .map(|body| serde_json::from_str::<Value>(body).unwrap_or_else(|_| json!(body)));
            (
                transform_codex_chat::chat_error_to_response_error(parsed_body.as_ref()),
                Some(*status),
            )
        }
        _ => (
            json!({
                "error": {
                    "message": get_error_message(error),
                    "type": "proxy_error",
                    "code": codex_proxy_error_code(error),
                    "param": Value::Null,
                }
            }),
            None,
        ),
    };

    let Some(error_obj) = body
        .get_mut("error")
        .and_then(|value| value.as_object_mut())
    else {
        return body;
    };

    let message = if upstream_status == Some(413) {
        // 413 来自上游渠道商的网关（典型是 nginx 的 client_max_body_size），不是 CC
        // Switch 本地代理的限制（本地 DefaultBodyLimit 已放到 200MB）。上游响应体往往是
        // 一整段 nginx HTML，对用户毫无价值，这里替换成明确指向上游 + 可操作的指引，
        // 避免「以为是 CC Switch 封装了 nginx / 是本地代理的锅」这种反复出现的误解。
        format!(
            concat!(
                "Upstream provider rejected the request with HTTP 413 (Payload Too Large). ",
                "The request body exceeds the upstream gateway's size limit; this is the ",
                "provider's server-side limit, not a CC Switch limit. ",
                "Provider: {provider}; model: {model}; endpoint: {endpoint}. ",
                "To recover, shrink the request: run /compact, remove large pasted logs or ",
                "inline images, or ask the provider to raise its request body limit ",
                "(e.g. nginx client_max_body_size)."
            ),
            provider = provider_name,
            model = request_model,
            endpoint = endpoint,
        )
    } else {
        let cause = error_obj
            .get("message")
            .and_then(|value| value.as_str())
            .map(ToString::to_string)
            .filter(|message| !message.trim().is_empty())
            .unwrap_or_else(|| get_error_message(error));
        let status_fragment = upstream_status
            .map(|status| format!("; upstream_status: HTTP {status}"))
            .unwrap_or_default();
        format!(
            "CC Switch local proxy failed while handling Codex endpoint {endpoint}. Provider: {provider_name}; model: {request_model}{status_fragment}; cause: {cause}"
        )
    };

    error_obj.insert(
        "message".to_string(),
        Value::String(compact_error_message(&message, 1800)),
    );

    if error_obj
        .get("type")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().is_empty())
        .unwrap_or(true)
    {
        error_obj.insert("type".to_string(), Value::String("proxy_error".to_string()));
    }

    if error_obj.get("code").map(Value::is_null).unwrap_or(true) {
        error_obj.insert(
            "code".to_string(),
            Value::String(codex_proxy_error_code(error).to_string()),
        );
    }

    if !error_obj.contains_key("param") {
        error_obj.insert("param".to_string(), Value::Null);
    }

    error_obj.insert(
        "provider".to_string(),
        Value::String(provider_name.to_string()),
    );
    error_obj.insert(
        "model".to_string(),
        Value::String(request_model.to_string()),
    );
    // 仅用于 Codex 本地路由；不要复用到 query 可能携带凭证的端点。
    error_obj.insert("endpoint".to_string(), Value::String(endpoint.to_string()));
    if let Some(status) = upstream_status {
        error_obj.insert(
            "upstream_status".to_string(),
            Value::Number(serde_json::Number::from(status)),
        );
    }

    body
}

fn codex_proxy_error_code(error: &ProxyError) -> &'static str {
    match error {
        ProxyError::ForwardFailed(_) => "cc_switch_forward_failed",
        ProxyError::Timeout(_) | ProxyError::StreamIdleTimeout(_) => "cc_switch_timeout",
        ProxyError::NoAvailableProvider => "cc_switch_no_available_provider",
        ProxyError::AllProvidersCircuitOpen => "cc_switch_all_providers_circuit_open",
        ProxyError::NoProvidersConfigured => "cc_switch_no_providers_configured",
        ProxyError::MaxRetriesExceeded => "cc_switch_max_retries_exceeded",
        ProxyError::ProviderUnhealthy(_) => "cc_switch_provider_unhealthy",
        ProxyError::ConfigError(_) => "cc_switch_config_error",
        ProxyError::TransformError(_) => "cc_switch_transform_error",
        ProxyError::InvalidRequest(_) => "cc_switch_invalid_request",
        ProxyError::AuthError(_) => "cc_switch_auth_error",
        ProxyError::UpstreamError { .. } => "cc_switch_upstream_error",
        ProxyError::DatabaseError(_) => "cc_switch_database_error",
        ProxyError::Internal(_) => "cc_switch_internal_error",
        ProxyError::AlreadyRunning
        | ProxyError::NotRunning
        | ProxyError::BindFailed(_)
        | ProxyError::StopTimeout
        | ProxyError::StopFailed(_)
        | ProxyError::ResponseBodyTooLarge(_) => "cc_switch_proxy_error",
    }
}

fn compact_error_message(message: &str, max_chars: usize) -> String {
    let normalized = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= max_chars {
        return normalized;
    }

    let truncated = normalized
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim_end()
        .to_string();
    format!("{truncated}…(truncated)")
}

pub fn proxy_error_response(
    provider_name: &str,
    model: &str,
    streaming: bool,
    error: &ProxyError,
) -> Response {
    let status = StatusCode::from_u16(map_proxy_error_to_status(error))
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = codex_proxy_error_json(provider_name, model, "/v1/responses", error);
    if let Some(response) = codex_context_window_sse_response(model, &body, status, streaming) {
        return response;
    }
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod upstream_error_tests {
    use super::*;
    #[test]
    fn codex_proxy_forward_error_includes_context_and_cause() {
        let error = ProxyError::ForwardFailed("连接失败: dns lookup failed".to_string());
        let body = codex_proxy_error_json("DeepSeek", "deepseek-chat", "/responses", &error);

        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("CC Switch local proxy failed"));
        assert!(message.contains("DeepSeek"));
        assert!(message.contains("deepseek-chat"));
        assert!(message.contains("/responses"));
        assert!(message.contains("dns lookup failed"));
        assert_eq!(body["error"]["code"], "cc_switch_forward_failed");
        assert_eq!(body["error"]["provider"], "DeepSeek");
        assert_eq!(body["error"]["model"], "deepseek-chat");
    }

    #[test]
    fn codex_proxy_upstream_error_normalizes_nonstandard_body() {
        let error = ProxyError::UpstreamError {
            status: 502,
            body: Some(
                r#"{"base_resp":{"status_code":2013,"status_msg":"upstream gateway failed"}}"#
                    .to_string(),
            ),
        };
        let body = codex_proxy_error_json("MiniMax", "abab6.5s", "/responses", &error);

        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("upstream_status: HTTP 502"));
        assert!(message.contains("upstream gateway failed"));
        assert_eq!(body["error"]["code"], 2013);
        assert_eq!(body["error"]["upstream_status"], 502);
    }

    #[test]
    fn codex_proxy_413_points_to_upstream_not_local_proxy() {
        // 模拟上游渠道商 nginx 因 client_max_body_size 返回的 413 HTML 页面
        // （见 issue #666：长上下文 / 大图 / 大日志撞上游体积上限）
        let error = ProxyError::UpstreamError {
            status: 413,
            body: Some(
                "<html>\r\n<head><title>413 Request Entity Too Large</title></head>\r\n\
                 <body>\r\n<center><h1>413 Request Entity Too Large</h1></center>\r\n\
                 <hr><center>nginx/1.29.6</center>\r\n</body>\r\n</html>"
                    .to_string(),
            ),
        };
        let body = codex_proxy_error_json("HCAI", "gpt-5.5", "/responses", &error);

        let message = body["error"]["message"].as_str().unwrap();
        // 不再误导成「本地代理失败」
        assert!(!message.contains("CC Switch local proxy failed"));
        // 明确指向上游 + 体积超限 + 可操作指引
        assert!(message.contains("413"));
        assert!(message.to_lowercase().contains("upstream"));
        assert!(message.contains("/compact"));
        // 关键：不把整段 nginx HTML 回显给用户
        assert!(!message.contains("<html>"));
        assert!(!message.contains("nginx/1.29.6"));
        // 结构化字段仍然保留，便于程序化消费 / UI 呈现
        assert_eq!(body["error"]["upstream_status"], 413);
        assert_eq!(body["error"]["provider"], "HCAI");
        assert_eq!(body["error"]["model"], "gpt-5.5");
        assert_eq!(body["error"]["endpoint"], "/responses");
    }
}
