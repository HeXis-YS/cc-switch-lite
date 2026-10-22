use crate::{
    compat::{request, response, retry, secrets},
    config::{Config, Protocol},
    proxy::{
        error::ProxyError,
        opaque_state_rectifier::{
            carries_encrypted_agent_task, detect_opaque_state_rejection,
            unreadable_agent_task_error,
        },
        providers::{
            codex_chat_history::CodexChatHistoryStore,
            transform_codex_chat::build_codex_tool_context_from_request,
        },
    },
};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::HeaderMap,
    response::Response,
    routing::post,
    Router,
};
use serde_json::Value;
use std::sync::Arc;

struct App {
    config: Config,
    client: reqwest::Client,
    history: Arc<CodexChatHistoryStore>,
}
pub fn router(config: Config) -> anyhow::Result<Router> {
    config.validate()?;
    // The upstream API-key path does not enable failover request/idle timeouts.
    // Redirects are surfaced as upstream errors, never another credential target.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    Ok(Router::new()
        .route("/v1/responses", post(responses))
        .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
        .with_state(Arc::new(App {
            config,
            client,
            history: Arc::default(),
        })))
}
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let address = config.server.listen;
    let app = router(config)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
async fn responses(
    State(app): State<Arc<App>>,
    mut headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> Response {
    let provider = app.config.selected();
    let decoded =
        if let Some(encoding) = crate::proxy::content_encoding::get_content_encoding(&headers) {
            if !crate::proxy::content_encoding::is_supported_content_encoding(&encoding) {
                Err(ProxyError::InvalidRequest(format!(
                    "Unsupported request content-encoding: {encoding}"
                )))
            } else {
                match crate::proxy::content_encoding::decompress_body(&encoding, &body) {
                    Ok(Some(bytes)) => {
                        headers.remove(axum::http::header::CONTENT_ENCODING);
                        headers.remove(axum::http::header::CONTENT_LENGTH);
                        headers.remove(axum::http::header::TRANSFER_ENCODING);
                        Ok(bytes::Bytes::from(bytes))
                    }
                    Ok(None) => Err(ProxyError::InvalidRequest(format!(
                        "Unsupported request content-encoding: {encoding}"
                    ))),
                    Err(error) => Err(ProxyError::InvalidRequest(format!(
                        "Failed to decompress request body ({encoding}): {error}"
                    ))),
                }
            }
        } else {
            Ok(body)
        };
    let input = decoded.and_then(|bytes| {
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| ProxyError::InvalidRequest("Invalid JSON request".into()))
    });
    let model = input
        .as_ref()
        .ok()
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let streaming = input
        .as_ref()
        .ok()
        .and_then(|v| v.get("stream"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let result = match input {
        Ok(input) => forward(&app, &headers, input, &model, streaming, uri.query()).await,
        Err(e) => Err(e),
    };
    let response = match result {
        Ok(r) => r,
        Err(e) => response::proxy_error_response(&provider.name, &model, streaming, &e),
    };
    secrets::protect(
        response,
        app.config
            .providers
            .iter()
            .map(|p| p.api_key.clone())
            .collect(),
    )
    .await
}
async fn forward(
    app: &App,
    headers: &HeaderMap,
    mut input: Value,
    model: &str,
    streaming: bool,
    query: Option<&str>,
) -> Result<Response, ProxyError> {
    let provider = app.config.selected();
    let mut retried = false;
    let mut attempt_headers = headers.clone();
    let context = build_codex_tool_context_from_request(&input);
    let mut stable_response_options = None;
    loop {
        let prepared = request::prepare_with_query(
            input.clone(),
            provider,
            &attempt_headers,
            &app.config.compat,
            &app.history,
            query,
        )
        .await?;
        if !attempt_headers.contains_key("x-opencode-session") {
            if let Some(session) = prepared.headers.get("x-opencode-session") {
                attempt_headers.insert("x-opencode-session", session.clone());
            }
        }
        if stable_response_options.is_none() {
            stable_response_options = Some((
                prepared.late_arguments_repair,
                prepared.native_compaction,
                prepared.xai_restore.clone(),
            ));
        }
        let attempted = send(app, &prepared).await;
        match attempted {
            Ok(upstream) => {
                let options = response::ResponseOptions {
                    model: model.into(),
                    streaming,
                    late_arguments_repair: stable_response_options.as_ref().unwrap().0,
                    native_compaction: stable_response_options.as_ref().unwrap().1,
                    xai_restore: stable_response_options.as_ref().unwrap().2.clone(),
                };
                return response::handle(
                    upstream,
                    provider.protocol,
                    options,
                    context,
                    app.history.clone(),
                )
                .await;
            }
            Err(e) => {
                if retried {
                    if provider.protocol == Protocol::Responses
                        && carries_encrypted_agent_task(&input)
                        && detect_opaque_state_rejection(
                            &e,
                            &app.config.compat.rectifier,
                            &input,
                            true,
                        )
                        .is_some()
                    {
                        return Err(unreadable_agent_task_error());
                    }
                    return Err(e);
                }
                if let Some(rectified) =
                    retry::rectify(provider.protocol, &input, &e, &app.config.compat.rectifier)?
                {
                    input = rectified;
                    retried = true;
                } else {
                    return Err(e);
                }
            }
        }
    }
}
async fn send(app: &App, prepared: &request::Prepared) -> Result<reqwest::Response, ProxyError> {
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
