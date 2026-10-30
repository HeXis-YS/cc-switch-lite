use axum::{
    body::Body, extract::State, http::StatusCode, response::Response, routing::post, Json, Router,
};
use cc_switch_lite::{
    config::{Config, DefaultRoute, Protocol, Provider, Server},
    server,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
struct Mock {
    calls: Arc<Mutex<Vec<Value>>>,
    body: String,
    sse: bool,
}
async fn upstream(State(mock): State<Mock>, Json(body): Json<Value>) -> Response {
    mock.calls.lock().await.push(body);
    let chunks = mock
        .body
        .into_bytes()
        .into_iter()
        .map(|b| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b])));
    Response::builder()
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
struct Tasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
async fn start(router: Router, tasks: &mut Tasks) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tasks.0.push(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    url
}
async fn setup(protocol: Protocol, body: String, sse: bool) -> (String, Config, Mock, Tasks) {
    let mock = Mock {
        calls: Arc::default(),
        body,
        sse,
    };
    let mut tasks = Tasks(vec![]);
    let url = start(
        Router::new()
            .route("/v1/chat/completions", post(upstream))
            .route("/v1/messages", post(upstream))
            .with_state(mock.clone()),
        &mut tasks,
    )
    .await;
    let config = Config {
        version: 1,
        compat: Default::default(),
        server: Server {
            listen: "127.0.0.1:0".parse().unwrap(),
        },
        default_route: Some(DefaultRoute {
            provider: "one".into(),
        }),
        routes: vec![],
        providers: vec![Provider {
            id: "one".into(),
            name: "One".into(),
            protocol,
            base_url: format!("{url}/v1"),
            api_key: "private-upstream-credential".into(),
            compat: Default::default(),
        }],
    };
    let proxy = start(server::router(config.clone()).unwrap(), &mut tasks).await;
    (proxy, config, mock, tasks)
}
async fn send(url: &str, value: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{url}/v1/responses"))
        .json(&value)
        .send()
        .await
        .unwrap()
}
fn sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect()
}
fn events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[tokio::test]
async fn streamed_chat_tool_history_is_restored_and_isolated_per_router() {
    let body = sse(vec![
        json!({"id":"chat_stream","model":"same","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_lookup","type":"function","function":{"name":"lookup","arguments":"{\"city\":"}}]}}]}),
        json!({"id":"chat_stream","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"Tokyo\"}"}}]}}]}),
        json!({"id":"chat_stream","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]) + "data: [DONE]\n\n";
    let (url, config, mock, mut tasks) = setup(Protocol::ChatCompletions, body, true).await;
    let text = send(
        &url,
        json!({"model":"same","input":"lookup city","stream":true}),
    )
    .await
    .text()
    .await
    .unwrap();
    let output = events(&text);
    let completed = output
        .iter()
        .find(|v| v["type"] == "response.completed")
        .expect("completion event");
    let call = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "function_call")
        .unwrap();
    assert_eq!(call["call_id"], "call_lookup");
    assert_eq!(
        serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Tokyo"})
    );
    let followup = json!({"model":"same","stream":true,"previous_response_id":completed["response"]["id"],"input":[{"type":"function_call_output","call_id":"call_lookup","output":"sunny"}]});
    assert_eq!(send(&url, followup.clone()).await.status(), StatusCode::OK);
    let calls = mock.calls.lock().await;
    let messages = calls[1]["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["role"] == "assistant" && m["tool_calls"][0]["id"] == "call_lookup"));
    assert!(messages
        .iter()
        .any(|m| m["role"] == "tool" && m["content"] == "sunny"));
    drop(calls);
    let fresh = start(server::router(config).unwrap(), &mut tasks).await;
    let _ = send(&fresh, followup).await.text().await.unwrap();
    let calls = mock.calls.lock().await;
    let fresh_messages = calls.last().unwrap()["messages"].as_array().unwrap();
    assert!(!fresh_messages
        .iter()
        .any(|m| m["tool_calls"][0]["id"] == "call_lookup"));
}

#[tokio::test]
async fn anthropic_json_thinking_and_tool_use_are_converted() {
    let body = json!({"id":"msg_tools","type":"message","role":"assistant","model":"same","content":[{"type":"thinking","thinking":"check the city","signature":"signature"},{"type":"tool_use","id":"call_weather","name":"weather","input":{"city":"Tokyo"}}],"stop_reason":"tool_use","usage":{"input_tokens":2,"output_tokens":4}}).to_string();
    let (url, _, mock, _tasks) = setup(Protocol::Messages, body, false).await;
    let response = send(&url, json!({"model":"same","input":"weather","tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    let output = body["output"].as_array().unwrap();
    let reasoning = output.iter().find(|v| v["type"] == "reasoning").unwrap();
    assert_eq!(reasoning["summary"][0]["text"], "check the city");
    assert!(reasoning["encrypted_content"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    let tool = output
        .iter()
        .find(|v| v["type"] == "function_call")
        .unwrap();
    assert_eq!(tool["call_id"], "call_weather");
    assert_eq!(tool["name"], "weather");
    assert_eq!(
        serde_json::from_str::<Value>(tool["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Tokyo"})
    );
    let calls = mock.calls.lock().await;
    assert_eq!(calls[0]["tools"][0]["name"], "weather");
    assert_eq!(calls[0]["tools"][0]["input_schema"]["type"], "object");
}

#[tokio::test]
async fn anthropic_success_status_error_envelope_fails_closed() {
    let body = json!({"type":"error","error":{"type":"overloaded_error","message":"private-upstream-credential"}}).to_string();
    let (url, _, _, _tasks) = setup(Protocol::Messages, body, false).await;
    let response = send(&url, json!({"model":"same","input":"hello"})).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response.text().await.unwrap();
    assert!(!body.contains("private-upstream-credential"));
    assert!(serde_json::from_str::<Value>(&body).unwrap()["error"].is_object());
}

#[tokio::test]
async fn anthropic_stream_converts_thinking_and_incremental_tool_arguments() {
    let body = sse(vec![
        json!({"type":"message_start","message":{"id":"msg_stream_tools","model":"same","usage":{"input_tokens":2}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"check city"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_weather","name":"weather","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"Tokyo\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ]);
    let (url, _, _, _tasks) = setup(Protocol::Messages, body, true).await;
    let text = send(
        &url,
        json!({"model":"same","input":"weather","stream":true}),
    )
    .await
    .text()
    .await
    .unwrap();
    let output = events(&text);
    assert!(output.iter().any(
        |v| v["type"] == "response.reasoning_summary_text.delta" && v["delta"] == "check city"
    ));
    let completed = output
        .iter()
        .find(|v| v["type"] == "response.completed")
        .expect("completion event");
    let tool = completed["response"]["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["type"] == "function_call")
        .unwrap();
    assert_eq!(tool["call_id"], "call_weather");
    assert_eq!(
        serde_json::from_str::<Value>(tool["arguments"].as_str().unwrap()).unwrap(),
        json!({"city":"Tokyo"})
    );
}
