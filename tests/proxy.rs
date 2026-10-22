use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use cc_switch_lite::{
    config::{Config, Protocol, Provider, Route, Server},
    server,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
struct Mock {
    calls: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    body: String,
    sse: bool,
    status: StatusCode,
}
async fn upstream(
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.calls.lock().await.push((headers, body));
    // Split every byte to exercise UTF-8, SSE framing and credential boundaries.
    let chunks = mock
        .body
        .into_bytes()
        .into_iter()
        .map(|b| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b])));
    Response::builder()
        .status(mock.status)
        .header(
            "content-type",
            if mock.sse {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}
async fn start(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (url, task)
}
fn config(url: &str, protocol: Protocol) -> Config {
    Config {
        version: 1,
        server: Server {
            listen: "127.0.0.1:0".parse().unwrap(),
        },
        route: Route {
            provider: "one".into(),
        },
        providers: vec![Provider {
            id: "one".into(),
            name: "One".into(),
            protocol,
            base_url: format!("{url}/v1"),
            api_key: "secret-credential-123".into(),
        }],
    }
}
async fn setup(
    protocol: Protocol,
    body: String,
    sse: bool,
    status: StatusCode,
) -> (String, Mock, Vec<tokio::task::JoinHandle<()>>) {
    let mock = Mock {
        calls: Arc::default(),
        body,
        sse,
        status,
    };
    let (url, u) = start(
        Router::new()
            .route("/v1/responses", post(upstream))
            .route("/v1/chat/completions", post(upstream))
            .route("/v1/messages", post(upstream))
            .with_state(mock.clone()),
    )
    .await;
    let (proxy, p) = start(server::router(config(&url, protocol)).unwrap()).await;
    (proxy, mock, vec![u, p])
}
async fn send(url: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&body)
        .send()
        .await
        .unwrap()
}
#[tokio::test]
async fn json_protocols_preserve_model_and_use_correct_auth() {
    for (protocol, body) in [
        (Protocol::Responses, json!({"id":"resp_test","output":[]})),
        (
            Protocol::ChatCompletions,
            json!({"id":"chat_test","model":"arbitrary-model","choices":[{"message":{"role":"assistant","content":"你好"},"finish_reason":"stop"}]}),
        ),
        (
            Protocol::Messages,
            json!({"id":"msg_test","type":"message","role":"assistant","model":"arbitrary-model","content":[{"type":"text","text":"你好"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}),
        ),
    ] {
        let (url, mock, tasks) = setup(protocol, body.to_string(), false, StatusCode::OK).await;
        let response=send(&url,json!({"model":"arbitrary-model","input":"hello","previous_response_id":"resp_previous"})).await;
        assert_eq!(response.status(), StatusCode::OK);
        let result: Value = response.json().await.unwrap();
        if protocol == Protocol::Responses {
            assert_eq!(result, body);
        } else {
            assert_eq!(result["output"][0]["content"][0]["text"], "你好");
        }
        let calls = mock.calls.lock().await;
        assert_eq!(calls[0].1["model"], "arbitrary-model");
        if protocol == Protocol::Messages {
            assert_eq!(calls[0].0["x-api-key"], "secret-credential-123");
            assert_eq!(calls[0].0["anthropic-version"], "2023-06-01");
        } else {
            assert_eq!(calls[0].0["authorization"], "Bearer secret-credential-123");
        }
        if protocol == Protocol::Responses {
            assert_eq!(calls[0].1["previous_response_id"], "resp_previous");
        }
        for task in tasks {
            task.abort();
        }
    }
}
#[tokio::test]
async fn sse_protocols_and_fragmented_credentials() {
    let native="event: response.completed\ndata: {\"type\":\"response.completed\",\"text\":\"你好 secret-credential-123\"}\n\n";
    let chat="data: {\"id\":\"chat_sse\",\"model\":\"same\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"}}]}\n\ndata: {\"id\":\"chat_sse\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let messages="event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sse\",\"model\":\"same\",\"usage\":{\"input_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    for (protocol, body) in [
        (Protocol::Responses, native),
        (Protocol::ChatCompletions, chat),
        (Protocol::Messages, messages),
    ] {
        let (url, _, tasks) = setup(protocol, body.into(), true, StatusCode::OK).await;
        let response = send(&url, json!({"model":"same","input":"hello","stream":true})).await;
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        let text = response.text().await.unwrap();
        assert!(text.contains("response.completed"), "{text}");
        assert!(text.contains("你好"));
        assert!(!text.contains("secret-credential-123"));
        if protocol == Protocol::Responses {
            assert_eq!(text, native.replace("secret-credential-123", "[REDACTED]"));
        }
        for task in tasks {
            task.abort();
        }
    }
}
#[tokio::test]
async fn chat_previous_response_restores_tool_call() {
    let body=json!({"id":"chat_tool","choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}).to_string();
    let (url, mock, tasks) = setup(Protocol::ChatCompletions, body, false, StatusCode::OK).await;
    let first: Value = send(&url, json!({"model":"same","input":"lookup"}))
        .await
        .json()
        .await
        .unwrap();
    let second=send(&url,json!({"model":"same","previous_response_id":first["id"],"input":[{"type":"function_call_output","call_id":"call_1","output":"found"}]})).await;
    assert_eq!(second.status(), StatusCode::OK);
    let calls = mock.calls.lock().await;
    let messages = calls[1].1["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|v| v["tool_calls"][0]["id"] == "call_1"));
    assert!(messages
        .iter()
        .any(|v| v["role"] == "tool" && v["content"] == "found"));
    for task in tasks {
        task.abort();
    }
}
#[tokio::test]
async fn errors_do_not_expose_upstream_credentials_or_redirect() {
    for protocol in [
        Protocol::Responses,
        Protocol::ChatCompletions,
        Protocol::Messages,
    ] {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::FOUND,
        ] {
            let (url, _, tasks) =
                setup(protocol, "secret-credential-123".into(), false, status).await;
            let response = send(&url, json!({"model":"same","input":"hello"})).await;
            assert_eq!(
                response.status(),
                if status.is_redirection() {
                    StatusCode::BAD_GATEWAY
                } else {
                    status
                }
            );
            assert!(!response
                .text()
                .await
                .unwrap()
                .contains("secret-credential-123"));
            for task in tasks {
                task.abort();
            }
        }
    }
}
#[tokio::test]
async fn running_router_uses_snapshot_until_recreated() {
    let mock = Mock {
        calls: Arc::default(),
        body: "{}".into(),
        sse: false,
        status: StatusCode::OK,
    };
    let (url, u) = start(
        Router::new()
            .route("/v1/responses", post(upstream))
            .with_state(mock.clone()),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let original = config(&url, Protocol::Responses);
    original.save(&path).unwrap();
    let (old, p) = start(server::router(Config::load(&path).unwrap()).unwrap()).await;
    let mut changed = original.clone();
    changed.providers[0].api_key = "new-credential".into();
    changed.save(&path).unwrap();
    send(&old, json!({"model":"same","input":"hi"})).await;
    let (new, n) = start(server::router(Config::load(&path).unwrap()).unwrap()).await;
    send(&new, json!({"model":"same","input":"hi"})).await;
    let calls = mock.calls.lock().await;
    assert_eq!(calls[0].0["authorization"], "Bearer secret-credential-123");
    assert_eq!(calls[1].0["authorization"], "Bearer new-credential");
    u.abort();
    p.abort();
    n.abort();
}

#[tokio::test]
async fn structured_upstream_errors_keep_type_code_and_scrub_secrets() {
    for protocol in [
        Protocol::Responses,
        Protocol::ChatCompletions,
        Protocol::Messages,
    ] {
        let body = r#"{"error":{"type":"authentication_error","code":"invalid_key","message":"rejected secret-credential-12\u0033"}}"#;
        let (url, _, tasks) = setup(protocol, body.into(), false, StatusCode::UNAUTHORIZED).await;
        let response = send(&url, json!({"model":"same","input":"hello"})).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["error"]["type"], "authentication_error");
        assert_eq!(value["error"]["code"], "invalid_key");
        assert_eq!(value["error"]["message"], "rejected [REDACTED]");
        for task in tasks {
            task.abort();
        }
    }
}
