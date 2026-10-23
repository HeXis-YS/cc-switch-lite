//! Send one attempt and validate its response before downstream commitment.
use super::App;
use crate::{
    compat::{request, response, retry},
    config::Protocol,
    proxy::{error::ProxyError, opaque_state_rectifier::carries_encrypted_agent_task},
};
pub(super) async fn send(
    app: &App,
    prepared: &request::Prepared,
) -> Result<reqwest::Response, ProxyError> {
    let provider = app.config.selected();
    let mut outgoing = app
        .client
        .post(&prepared.url)
        .headers(prepared.headers.clone())
        .body(
            serde_json::to_vec(&prepared.body)
                .map_err(|e| ProxyError::TransformError(e.to_string()))?,
        );
    // Preserve forwarder defaults even with failover disabled: streaming sends
    // have a header deadline and a long total deadline, but no idle deadline.
    if prepared.request_is_streaming {
        outgoing = outgoing.timeout(std::time::Duration::from_secs(24 * 60 * 60));
    }
    let send = outgoing.send();
    let result = if prepared.request_is_streaming {
        tokio::time::timeout(std::time::Duration::from_secs(600), send)
            .await
            .map_err(|_| ProxyError::Timeout("流式响应首包超时: 600s（上游未返回响应头）".into()))?
    } else {
        send.await
    };
    let upstream = result.map_err(|error| {
        let is_timeout = error.is_timeout();
        let is_connect = error.is_connect();
        let detail = crate::proxy::error::describe_reqwest_error(error);
        if is_timeout {
            ProxyError::Timeout(format!("上游请求超时: {detail}"))
        } else if is_connect {
            ProxyError::ForwardFailed(format!("上游连接失败: {detail}"))
        } else {
            ProxyError::ForwardFailed(format!("上游请求发送失败: {detail}"))
        }
    })?;
    let status = upstream.status();
    if !status.is_success() {
        let (_, status, bytes) = response::decode_response(upstream).await?;
        return Err(ProxyError::UpstreamError {
            status: status.as_u16(),
            body: String::from_utf8(bytes.to_vec()).ok(),
        });
    }
    let is_json = upstream
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let mime = v
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            mime == "application/json" || mime.ends_with("+json")
        });
    if provider.protocol == Protocol::Messages && (!prepared.request_is_streaming || is_json) {
        let (headers, status, bytes) = response::decode_response(upstream).await?;
        retry::validate_anthropic_body(&bytes)?;
        let mut response = http::Response::new(bytes);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return Ok(response.into());
    }
    if provider.protocol == Protocol::Responses
        && prepared.request_is_streaming
        && !is_json
        && app.config.compat.rectifier.enabled
        && carries_encrypted_agent_task(&prepared.body)
    {
        return retry::hold_encrypted_agent_task_stream(upstream, &prepared.body).await;
    }
    Ok(upstream)
}
