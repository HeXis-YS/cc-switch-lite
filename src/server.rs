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

mod transport;

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
        // Conversion uses the original client flags and first attempt's repairs,
        // even when a retry rectifies the upstream request body.
        if stable_response_options.is_none() {
            stable_response_options = Some(response::ResponseOptions {
                model: model.into(),
                streaming,
                late_arguments_repair: prepared.late_arguments_repair,
                native_compaction: prepared.native_compaction,
                xai_restore: prepared.xai_restore.clone(),
            });
        }
        let attempted = transport::send(app, &prepared).await;
        match attempted {
            Ok(upstream) => {
                let options = stable_response_options
                    .take()
                    .expect("first attempt prepared response options");
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
