use crate::config::{Config, Protocol};
use crate::proxy::providers::{
    codex_chat_history::{record_responses_sse_stream, CodexChatHistoryStore},
    streaming_codex_anthropic::*,
    streaming_codex_chat::*,
    transform_codex_anthropic::*,
    transform_codex_chat::*,
};
use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

struct App {
    config: Config,
    client: reqwest::Client,
    history: Arc<CodexChatHistoryStore>,
}
pub fn router(config: Config) -> anyhow::Result<Router> {
    config.validate()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()?;
    Ok(Router::new()
        .route("/v1/responses", post(responses))
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
fn error(status: StatusCode, message: &'static str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"proxy_error","message":message}})),
    )
        .into_response()
}
async fn responses(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let mut input: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid JSON request"),
    };
    if !input
        .get("model")
        .and_then(Value::as_str)
        .is_some_and(|m| !m.is_empty())
    {
        return error(StatusCode::BAD_REQUEST, "A nonempty model is required");
    }
    let streaming = input
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let p = app.config.selected();
    if p.protocol == Protocol::ChatCompletions {
        app.history.enrich_request(&mut input).await;
    }
    let context = build_codex_tool_context_from_request(&input);
    let converted = match p.protocol {
        Protocol::Responses => Ok(input),
        Protocol::ChatCompletions => responses_to_chat_completions(input),
        Protocol::Messages => responses_request_to_anthropic(input, 8192),
    };
    let converted = match converted {
        Ok(v) => v,
        Err(_) => {
            return error(
                StatusCode::BAD_REQUEST,
                "Request cannot be converted to upstream protocol",
            )
        }
    };
    let endpoint = match p.protocol {
        Protocol::Responses => "responses",
        Protocol::ChatCompletions => "chat/completions",
        Protocol::Messages => "messages",
    };
    let mut request = app
        .client
        .post(format!("{}/{endpoint}", p.base_url.trim_end_matches('/')));
    request = if p.protocol == Protocol::Messages {
        request
            .header("x-api-key", &p.api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        request.bearer_auth(&p.api_key)
    };
    let request = if p.protocol == Protocol::Responses {
        request
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
    } else {
        request.json(&converted)
    };
    let upstream = match request.send().await {
        Ok(r) => r,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream connection failed"),
    };
    let status = upstream.status();
    let is_sse = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    // Sanitize before converters see data: their error paths may include upstream text.
    let secrets = app
        .config
        .providers
        .iter()
        .flat_map(|p| {
            let escaped = serde_json::to_string(&p.api_key).expect("string serialization");
            [p.api_key.clone(), escaped[1..escaped.len() - 1].to_string()]
        })
        .collect();
    let source = redact_stream(upstream.bytes_stream(), secrets);
    if !status.is_success() {
        let status = if status.is_redirection() {
            StatusCode::BAD_GATEWAY
        } else {
            status
        };
        tokio::pin!(source);
        let mut data = Vec::new();
        while let Some(chunk) = source.next().await {
            match chunk {
                Ok(bytes) if data.len() + bytes.len() <= 1024 * 1024 => data.extend(bytes),
                _ => return error(status, "Upstream rejected request"),
            }
        }
        let mut value: Value = match serde_json::from_slice(&data) {
            Ok(value) => value,
            Err(_) => return error(status, "Upstream rejected request"),
        };
        // Decode JSON escapes before the final scrub, including Unicode escapes.
        scrub_value(&mut value, &app.config);
        return (status, Json(chat_error_to_response_error(Some(&value)))).into_response();
    }
    if streaming || is_sse {
        let body = match p.protocol {
            Protocol::Responses => Body::from_stream(source),
            Protocol::ChatCompletions => Body::from_stream(record_responses_sse_stream(
                create_responses_sse_stream_from_chat_with_context(source, context),
                app.history.clone(),
            )),
            Protocol::Messages => Body::from_stream(
                create_responses_sse_stream_from_anthropic_with_context(source, context),
            ),
        };
        return Response::builder()
            .status(status)
            .header(
                header::CONTENT_TYPE,
                if is_sse || p.protocol != Protocol::Responses {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap();
    }
    tokio::pin!(source);
    let mut data = Vec::new();
    while let Some(chunk) = source.next().await {
        match chunk {
            Ok(bytes) => data.extend(bytes),
            Err(_) => return error(StatusCode::BAD_GATEWAY, "Upstream body failed"),
        }
        if data.len() > 32 * 1024 * 1024 {
            return error(
                StatusCode::BAD_GATEWAY,
                "Upstream JSON response exceeds limit",
            );
        }
    }
    if p.protocol == Protocol::Responses {
        return (status, [(header::CONTENT_TYPE, "application/json")], data).into_response();
    }
    let value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "Invalid upstream JSON"),
    };
    let result = match p.protocol {
        Protocol::ChatCompletions => chat_completion_to_response_with_context(value, &context),
        Protocol::Messages => anthropic_response_to_responses_with_context(value, &context),
        Protocol::Responses => unreachable!(),
    };
    match result {
        Ok(value) => {
            if p.protocol == Protocol::ChatCompletions {
                app.history.record_response(&value).await;
            }
            (status, Json(value)).into_response()
        }
        Err(_) => error(StatusCode::BAD_GATEWAY, "Invalid upstream response"),
    }
}

// Keep only a suffix that might be a split credential. Normal SSE chunks flush immediately.
fn redact_stream<E: std::error::Error + Send + 'static>(
    source: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    secrets: Vec<String>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        tokio::pin!(source);
        let mut pending = Vec::new();
        while let Some(chunk) = source.next().await {
            let chunk = match chunk {Ok(c)=>c,Err(_)=>{yield Err(std::io::Error::other("Upstream stream failed"));return;}};
            pending.extend_from_slice(&chunk);
            let mut output = Vec::new();
            let mut i = 0;
            while i < pending.len() {
                let tail = &pending[i..];
                if let Some(key) = secrets.iter().filter(|k| !k.is_empty()).find(|k| tail.starts_with(k.as_bytes())) {output.extend_from_slice(b"[REDACTED]");i+=key.len();}
                else if secrets.iter().any(|k| k.as_bytes().starts_with(tail)) {break;}
                else {output.push(pending[i]);i+=1;}
            }
            pending.drain(..i);
            if !output.is_empty() {yield Ok(Bytes::from(output));}
        }
        if !pending.is_empty() {yield Ok(Bytes::from(pending));}
    }
}

fn scrub_value(value: &mut Value, config: &Config) {
    match value {
        Value::String(text) => {
            for provider in &config.providers {
                *text = text.replace(&provider.api_key, "[REDACTED]");
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_value(item, config);
            }
        }
        Value::Object(object) => {
            let original = std::mem::take(object);
            for (mut key, mut item) in original {
                for provider in &config.providers {
                    key = key.replace(&provider.api_key, "[REDACTED]");
                }
                scrub_value(&mut item, config);
                object.insert(key, item);
            }
        }
        _ => {}
    }
}
