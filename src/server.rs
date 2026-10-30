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

    let provider = app.config.route_for_model(&model);
    let result = match (input, provider) {
        (Err(e), _) => Err(e),
        (Ok(_), _) if model.trim().is_empty() => Err(ProxyError::InvalidRequest(
            "model must be a nonempty string".into(),
        )),
        (Ok(input), Some(provider)) => {
            forward(
                &app,
                provider,
                &headers,
                input,
                &model,
                streaming,
                uri.query(),
            )
            .await
        }
        (Ok(_), None) => Err(ProxyError::InvalidRequest(
            "No route matches model and no default route is configured".into(),
        )),
    };
    let response = match result {
        Ok(r) => r,
        Err(e) => response::proxy_error_response(
            provider.map_or("unselected", |p| p.name.as_str()),
            &model,
            streaming,
            &e,
        ),
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
    provider: &crate::config::Provider,
    headers: &HeaderMap,
    mut input: Value,
    model: &str,
    streaming: bool,
    query: Option<&str>,
) -> Result<Response, ProxyError> {
    let mut retried = false;
    let context = build_codex_tool_context_from_request(&input);
    let mut request_context =
        request::build_context(provider.protocol, provider, headers, &input, query);
    let stable_response_options = request_context.response_options(model, streaming);
    loop {
        let prepared = request::prepare_attempt(
            &mut request_context,
            input.clone(),
            provider,
            headers,
            &app.config.compat,
            &app.history,
        )
        .await?;
        let attempted = transport::send(app, provider.protocol, &prepared).await;
        match attempted {
            Ok(upstream) => {
                return response::handle(
                    upstream,
                    provider.protocol,
                    stable_response_options,
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
