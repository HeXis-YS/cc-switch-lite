//! Same-provider media/opaque retry boundaries from cc-switch 214e75ba (MIT).
use crate::config::Protocol;
use crate::proxy::{
    error::ProxyError, media_sanitizer, opaque_state_rectifier::*, types::RectifierConfig,
};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::Value;

/// The Codex adapter never enters Claude-only thinking signature/budget retries.
/// The first applicable recovery owns one retry; a failed retry is final in the
/// single-provider branch, rather than cascading into another recovery.
pub fn rectify(
    protocol: Protocol,
    request: &Value,
    error: &ProxyError,
    config: &RectifierConfig,
) -> Result<Option<Value>, ProxyError> {
    if config.enabled
        && config.request_media_fallback
        && media_sanitizer::contains_image_blocks(request)
        && media_sanitizer::is_unsupported_image_error(error)
    {
        let mut body = request.clone();
        if media_sanitizer::replace_image_blocks_with_marker(&mut body) > 0 {
            return Ok(Some(body));
        }
    }
    if protocol == Protocol::Responses {
        if let Some(rejection) = detect_opaque_state_rejection(error, config, request, true) {
            let agent_task = carries_encrypted_agent_task(request);
            let mut body = request.clone();
            let result = rectify_opaque_state(&mut body, rejection);
            if !result.applied && agent_task {
                return Err(unreadable_agent_task_error());
            }
            if result.applied {
                return Ok(Some(body));
            }
        }
    }
    Ok(None)
}
pub fn validate_anthropic_body(body: &[u8]) -> Result<(), ProxyError> {
    if let Some(message) = codex_anthropic_error_envelope_message(body) {
        return Err(ProxyError::TransformError(format!(
            "Anthropic upstream returned a 2xx error envelope: {message}"
        )));
    }
    Ok(())
}
fn codex_anthropic_error_envelope_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") && value.get("error").is_none() {
        return None;
    }
    let error = value.get("error").unwrap_or(&value);
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("error");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
    Some(format!("{error_type}: {message}"))
}

enum StreamStart {
    /// 生命周期事件（或不是 JSON），接着读。
    Pending,
    /// 有输出或别的结局：交出流。
    Commit,
    /// 输出之前上游就自证验不了请求里的密文。
    Rejected(ProxyError),
}

fn encrypted_task_stream_start(block: &str, request: &Value) -> StreamStart {
    let mut named_event = None;
    let mut data_lines = Vec::new();
    for line in block.lines() {
        if let Some(event) = crate::proxy::sse::strip_sse_field(line, "event") {
            named_event = Some(event.trim().to_string());
        } else if let Some(data) = crate::proxy::sse::strip_sse_field(line, "data") {
            data_lines.push(data);
        }
    }
    let Ok(value) = serde_json::from_str::<Value>(&data_lines.join("\n")) else {
        return StreamStart::Pending;
    };
    let event = named_event
        .as_deref()
        .filter(|event| !event.is_empty())
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or("");
    match event {
        "response.created" | "response.in_progress" | "response.queued" => StreamStart::Pending,
        "response.failed" | "error" => {
            let response = value.get("response").unwrap_or(&value);
            let error = response.get("error").unwrap_or(response);
            match crate::proxy::opaque_state_rectifier::in_stream_rejection(error, request) {
                Some(rejection) => StreamStart::Rejected(rejection),
                None => StreamStart::Commit,
            }
        }
        _ => StreamStart::Commit,
    }
}

pub async fn hold_encrypted_agent_task_stream(
    response: reqwest::Response,
    request: &Value,
) -> Result<reqwest::Response, ProxyError> {
    const MAX_HOLD_BYTES: usize = 256 * 1024;
    const MAX_HOLD: std::time::Duration = std::time::Duration::from_secs(10);

    let window = MAX_HOLD;
    let deadline = tokio::time::Instant::now() + window;
    let status = response.status();
    let headers = response.headers().clone();
    let mut stream = Box::pin(response.bytes_stream());
    let mut replay_chunks: Vec<Bytes> = Vec::new();
    let mut held_bytes = 0usize;
    let mut parse_buffer = String::new();
    let mut utf8_remainder = Vec::new();

    'hold: loop {
        let Ok(next) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break 'hold;
        };
        let Some(chunk) = next else {
            break 'hold;
        };
        // 还没有交出流，读出错和首包前出错一样回到重试循环。
        let chunk = chunk.map_err(|error| {
            ProxyError::ForwardFailed(format!(
                "读取流式响应失败: {}",
                crate::proxy::error::describe_reqwest_error(error)
            ))
        })?;
        crate::proxy::sse::append_utf8_safe(&mut parse_buffer, &mut utf8_remainder, &chunk);
        held_bytes += chunk.len();
        replay_chunks.push(chunk);

        while let Some(block) = crate::proxy::sse::take_sse_block(&mut parse_buffer) {
            match encrypted_task_stream_start(&block, request) {
                StreamStart::Pending => {}
                StreamStart::Commit => break 'hold,
                StreamStart::Rejected(error) => return Err(error),
            }
        }
        if held_bytes > MAX_HOLD_BYTES {
            break 'hold;
        }
    }

    let replay = futures::stream::iter(replay_chunks.into_iter().map(Ok)).chain(stream);
    let mut response = http::Response::new(reqwest::Body::wrap_stream(replay));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response.into())
}
