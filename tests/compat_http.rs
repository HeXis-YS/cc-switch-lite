//! End-to-end compatibility checks through real proxy and mock HTTP sockets.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use cc_switch_lite::{
    config::{Config, DefaultRoute, ModelMappingRule, Protocol, Provider, RouteRule, Server},
    server,
};
use serde_json::{json, Value};
use std::{io::Write, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}
impl Reply {
    fn new(status: StatusCode, kind: Option<&str>, body: impl Into<Vec<u8>>) -> Self {
        let mut headers = HeaderMap::new();
        if let Some(kind) = kind {
            headers.insert("content-type", kind.parse().unwrap());
        }
        Self {
            status,
            headers,
            body: body.into(),
        }
    }
    fn json(status: StatusCode, value: Value) -> Self {
        Self::new(status, Some("application/json"), value.to_string())
    }
    fn gzip(mut self) -> Self {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&self.body).unwrap();
        self.body = encoder.finish().unwrap();
        self.headers
            .insert("content-encoding", "gzip".parse().unwrap());
        self
    }
}
#[derive(Clone)]
struct Mock {
    calls: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    replies: Arc<Vec<Reply>>,
}
#[derive(Clone)]
struct ObservedRequest {
    uri: String,
    headers: HeaderMap,
}
async fn upstream(
    State(mock): State<Mock>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    mock.requests.lock().await.push(ObservedRequest {
        uri: uri.to_string(),
        headers,
    });
    let mut calls = mock.calls.lock().await;
    let i = calls.len();
    calls.push(body);
    let reply = &mock.replies[i.min(mock.replies.len() - 1)];
    let mut result = Response::new(Body::from(reply.body.clone()));
    *result.status_mut() = reply.status;
    *result.headers_mut() = reply.headers.clone();
    result
}
async fn start(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, handle)
}
struct Fixture {
    url: String,
    mock: Mock,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn new(
        protocol: Protocol,
        replies: Vec<Reply>,
        configure: impl FnOnce(&mut Config),
    ) -> Self {
        let mock = Mock {
            calls: Arc::default(),
            requests: Arc::default(),
            replies: Arc::new(replies),
        };
        let (url, upstream_task) = start(
            Router::new()
                .route("/v1/responses", post(upstream))
                .route("/v1/chat/completions", post(upstream))
                .route("/v1/messages", post(upstream))
                .route("/v1beta/models/:model_action", post(upstream))
                .layer(DefaultBodyLimit::max(200 * 1024 * 1024))
                .with_state(mock.clone()),
        )
        .await;
        let mut config = Config {
            version: 1,
            compat: Default::default(),
            server: Server {
                listen: "127.0.0.1:0".parse().unwrap(),
            },
            default_route: Some(DefaultRoute {
                provider: "one".into(),
            }),
            model_mappings: vec![],
            routes: vec![],
            providers: vec![Provider {
                id: "one".into(),
                name: "Mock".into(),
                protocol,
                base_url: format!("{url}/v1"),
                api_key: "test-key-not-for-output".into(),
                compat: Default::default(),
            }],
        };
        configure(&mut config);
        let (url, proxy_task) = start(server::router(config).unwrap()).await;
        Self {
            url,
            mock,
            tasks: vec![upstream_task, proxy_task],
        }
    }
    async fn send(&self, input: Value) -> reqwest::Response {
        self.send_with_headers_and_query(input, None, &[]).await
    }
    async fn send_with_headers_and_query(
        &self,
        input: Value,
        query: Option<&str>,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let query = query.map(|value| format!("?{value}")).unwrap_or_default();
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/responses{query}", self.url))
            .json(&input);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.send().await.unwrap()
    }
    async fn send_body(&self, body: Vec<u8>, content_encoding: Option<&str>) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/responses", self.url))
            .header("content-type", "application/json");
        if let Some(encoding) = content_encoding {
            request = request.header("content-encoding", encoding);
        }
        request.body(body).send().await.unwrap()
    }
}
fn input(stream: bool) -> Value {
    json!({"model":"compat-test-model","stream":stream,"input":"hello"})
}
fn chat_success() -> Value {
    json!({"id":"chat1","model":"compat-test-model","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]})
}
fn native_success() -> Reply {
    Reply::json(
        StatusCode::OK,
        json!({"id":"resp1","object":"response","status":"completed","output":[]}),
    )
}
fn opaque_error() -> Reply {
    Reply::json(
        StatusCode::BAD_REQUEST,
        json!({"error":{"type":"invalid_request_error","code":"invalid_encrypted_content","message":"Could not decrypt encrypted_content"}}),
    )
}
fn agent_task() -> Value {
    json!({"type":"agent_message","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"Message Type: NEW_TASK\nPayload:\n"},{"type":"encrypted_content","encrypted_content":"current-secret-task"}]})
}

#[tokio::test]
async fn incomplete_json_response_does_not_commit_tool_calls() {
    for (protocol, streaming) in [
        (Protocol::ChatCompletions, false),
        (Protocol::Messages, false),
        (Protocol::Messages, true),
    ] {
        for filtered in [false, true] {
            let body = if protocol == Protocol::ChatCompletions {
                json!({"id":"fixed","choices":[{"message":{"tool_calls":[{
                    "id":"call_cut","type":"function","function":{"name":"delete_file","arguments":"{}"}
                }]},"finish_reason":if filtered {"content_filter"} else {"length"}}]})
            } else {
                json!({"id":"fixed","type":"message","content":[{
                    "type":"tool_use","id":"call_cut","name":"delete_file","input":{}
                }],"stop_reason":if filtered {"refusal"} else {"max_tokens"}})
            };
            let fixture =
                Fixture::new(protocol, vec![Reply::json(StatusCode::OK, body)], |_| {}).await;
            let response = fixture.send(input(streaming)).await;
            assert_eq!(response.status(), StatusCode::OK);
            let text = response.text().await.unwrap();
            let response = if streaming {
                assert!(!text.contains("event: response.output_item.done"));
                text.split("\n\n")
                    .filter_map(|block| block.lines().find_map(|line| line.strip_prefix("data: ")))
                    .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                    .find(|event| event["type"] == "response.incomplete")
                    .unwrap()["response"]
                    .clone()
            } else {
                serde_json::from_str::<Value>(&text).unwrap()
            };
            assert_eq!(response["status"], "incomplete");
            assert_eq!(response["output"], json!([]));
            assert_eq!(
                response["incomplete_details"]["reason"],
                if filtered {
                    "content_filter"
                } else {
                    "max_output_tokens"
                }
            );
        }
    }
}

#[tokio::test]
async fn namespace_custom_tools_round_trip_through_chat_and_messages() {
    for protocol in [Protocol::ChatCompletions, Protocol::Messages] {
        for streaming in [false, true] {
            let call = json!({"id":"call_patch","type":"function","function":{
                "name":"functions__apply_patch","arguments":"{\"input\":\"patch text\"}"}});
            let tool = json!({"type":"tool_use","id":"call_patch","name":"functions__apply_patch",
                "input":{"input":"patch text"}});
            let reply = match (protocol,streaming) {
                (Protocol::ChatCompletions,false) => Reply::json(StatusCode::OK,json!({"id":"fixed",
                    "choices":[{"message":{"tool_calls":[call]},"finish_reason":"tool_calls"}]})),
                (Protocol::ChatCompletions,true) => Reply::new(StatusCode::OK,Some("text/event-stream"),
                    format!("data: {}\n\ndata: [DONE]\n\n",json!({"id":"fixed","choices":[{
                        "delta":{"tool_calls":[{"index":0,"id":"call_patch","type":"function","function":call["function"]}]},
                        "finish_reason":"tool_calls"}]})).into_bytes()),
                (Protocol::Messages,false) => Reply::json(StatusCode::OK,json!({"id":"fixed","type":"message",
                    "content":[tool],"stop_reason":"tool_use"})),
                (Protocol::Messages,true) => Reply::new(StatusCode::OK,Some("text/event-stream"),[
                    json!({"type":"message_start","message":{"id":"fixed"}}),
                    json!({"type":"content_block_start","index":0,"content_block":tool}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
                    json!({"type":"message_stop"})
                ].iter().map(|value|format!("data: {value}\n\n")).collect::<String>().into_bytes()),
                _ => unreachable!(),
            };
            let fixture = Fixture::new(protocol, vec![reply], |_| {}).await;
            let mut request = input(streaming);
            request["tools"] = json!([{"type":"namespace","name":"functions","tools":[{
                "type":"custom","name":"apply_patch","description":"Apply a patch","format":{"type":"text"}}
            ]}]);
            request["tool_choice"] =
                json!({"type":"custom","name":"apply_patch","namespace":"functions"});
            request["input"] = json!([
                {"role":"user","content":"Patch it"},
                {"type":"custom_tool_call","namespace":"functions","name":"apply_patch","call_id":"old_patch","input":"old patch"},
                {"type":"custom_tool_call_output","call_id":"old_patch","output":"ok"}
            ]);
            let response = fixture.send(request).await;
            assert_eq!(response.status(), StatusCode::OK);
            let text = response.text().await.unwrap();
            let item = if streaming {
                text.split("\n\n")
                    .filter_map(|block| block.lines().find_map(|line| line.strip_prefix("data: ")))
                    .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                    .find(|event| event["type"] == "response.output_item.done")
                    .unwrap()["item"]
                    .clone()
            } else {
                serde_json::from_str::<Value>(&text).unwrap()["output"][0].clone()
            };
            assert_eq!(item["type"], "custom_tool_call");
            assert_eq!(item["namespace"], "functions");
            assert_eq!(item["name"], "apply_patch");
            assert_eq!(item["input"], "patch text");
            let calls = fixture.mock.calls.lock().await;
            let sent = &calls[0];
            if protocol == Protocol::ChatCompletions {
                assert_eq!(
                    sent["tools"][0]["function"]["name"],
                    "functions__apply_patch"
                );
                assert_eq!(
                    sent["tool_choice"]["function"]["name"],
                    "functions__apply_patch"
                );
                assert!(sent["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message.pointer("/tool_calls/0/function/name")
                        == Some(&json!("functions__apply_patch"))));
            } else {
                assert_eq!(sent["tools"][0]["name"], "functions__apply_patch");
                assert_eq!(sent["tool_choice"]["name"], "functions__apply_patch");
                assert!(sent["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["content"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|block| block["type"] == "tool_use"
                            && block["name"] == "functions__apply_patch")));
            }
        }
    }
}

#[tokio::test]
async fn wrapped_chat_response_preserves_text_usage_and_tool_history() {
    let wrapped = json!({"success":true,"data":{
        "id":"wrapped","model":"compat-test-model","created":42,
        "choices":[{"message":{"role":"assistant","content":"checking",
            "tool_calls":[{"id":"call_wrapped","type":"function","function":{"name":"read","arguments":"{}"}}]},
            "finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":12,"completion_tokens":3}
    }});
    let fixture = Fixture::new(
        Protocol::ChatCompletions,
        vec![
            Reply::json(StatusCode::OK, wrapped),
            Reply::json(StatusCode::OK, chat_success()),
        ],
        |_| {},
    )
    .await;
    let response = fixture.send(input(false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["model"], "compat-test-model");
    assert_eq!(value["created_at"], 42);
    assert!(value["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["content"][0]["text"] == "checking"));
    assert_eq!(value["usage"]["input_tokens"], 12);
    assert_eq!(value["usage"]["output_tokens"], 3);
    let response = fixture
        .send(json!({"model":"compat-test-model","stream":false,
            "previous_response_id":value["id"],
            "input":[{"type":"function_call_output","call_id":"call_wrapped","output":"ok"}]
        }))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 2);
    let messages = calls[1]["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|m| m["tool_calls"][0]["id"] == "call_wrapped"));
    assert!(messages
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_wrapped"));
}

#[tokio::test]
async fn azure_resource_rejection_rectifies_once_and_respects_disabled_and_exhausted_boundaries() {
    for (enabled, recovery_success, expected_calls) in
        [(true, true, 2), (false, true, 1), (true, false, 2)]
    {
        let failure = Reply::json(
            StatusCode::BAD_REQUEST,
            json!({"error":{
                "type":"invalid_request_error","param":"input","code":null,
                "message":"The requested item was created under a different Azure OpenAI resource. Use the same resource that created the item to access it."
            }}),
        );
        let second = if recovery_success {
            native_success()
        } else {
            failure.clone()
        };
        let fixture = Fixture::new(Protocol::Responses, vec![failure, second], |config| {
            config.compat.rectifier.enabled = enabled;
        })
        .await;
        let request = json!({"model":"compat-test-model","input":[
            {"type":"message","role":"user","id":"msg_foreign","content":"hi"},
            {"type":"reasoning","id":"rs_foreign","summary":[],"encrypted_content":"foreign"},
            {"type":"function_call","id":"fc_foreign","call_id":"call_1","name":"read","arguments":"{\"id\":1}"},
            {"type":"function_call_output","call_id":"call_1","output":"ok"},
            {"type":"item_reference","id":"msg_reference"},
            {"type":"message","role":"user","content":"next"}
        ]});
        let response = fixture.send(request).await;
        assert_eq!(
            response.status(),
            if enabled && recovery_success {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let calls = fixture.mock.calls.lock().await;
        assert_eq!(calls.len(), expected_calls);
        assert_eq!(calls[0]["input"][0]["id"], "msg_foreign");
        if enabled {
            assert_eq!(
                calls[1]["input"],
                json!([
                    {"type":"message","role":"user","content":"hi"},
                    {"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"id\":1}"},
                    {"type":"function_call_output","call_id":"call_1","output":"ok"},
                    {"type":"item_reference","id":"msg_reference"},
                    {"type":"message","role":"user","content":"next"}
                ])
            );
        }
    }
}

#[tokio::test]
async fn unmarked_sse_bodies_are_aggregated_in_both_transformed_protocols() {
    let chat="data: {\"id\":\"c1\",\"model\":\"compat-test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n";
    let messages="event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg1\",\"model\":\"compat-test-model\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"hello\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    for (protocol, body) in [
        (Protocol::ChatCompletions, chat),
        (Protocol::Messages, messages),
    ] {
        let fixture = Fixture::new(
            protocol,
            vec![Reply::new(StatusCode::OK, None, body)],
            |_| {},
        )
        .await;
        let response = fixture.send(input(false)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["output"][0]["content"][0]["text"], "hello");
    }
}

#[tokio::test]
async fn messages_stream_with_complete_open_tool_input_repairs_missing_terminal_events() {
    let messages = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_tail\",\"model\":\"compat-test-model\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_tail\",\"name\":\"weather\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Tokyo\\\"}\"}}\n\n"
    );
    let fixture = Fixture::new(
        Protocol::Messages,
        vec![Reply::new(
            StatusCode::OK,
            Some("text/event-stream"),
            messages,
        )],
        |_| {},
    )
    .await;

    let response = fixture.send(input(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.function_call_arguments.done"));
    assert!(body.contains("event: response.output_item.done"));
    assert!(body.contains("Tokyo"));
    assert!(body.contains("event: response.completed"));
    assert!(!body.contains("event: response.incomplete"));
}

#[tokio::test]
async fn compressed_success_and_error_bodies_are_decoded_before_conversion() {
    let fixture = Fixture::new(
        Protocol::ChatCompletions,
        vec![Reply::json(StatusCode::OK, chat_success()).gzip()],
        |_| {},
    )
    .await;
    let response = fixture.send(input(false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key("content-encoding"));
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["output"][0]["content"][0]["text"], "hello");
    let fixture = Fixture::new(
        Protocol::Messages,
        vec![Reply::new(
            StatusCode::BAD_GATEWAY,
            Some("text/plain"),
            "diagnostic from gateway",
        )
        .gzip()],
        |_| {},
    )
    .await;
    let response = fixture.send(input(false)).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("diagnostic from gateway"));
}

#[tokio::test]
async fn compressed_codex_request_is_decoded_before_json_and_upstream_forwarding() {
    let request = input(false);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    let compressed = encoder.finish().unwrap();

    let fixture = Fixture::new(Protocol::Responses, vec![native_success()], |_| {}).await;
    let response = fixture.send_body(compressed, Some("gzip")).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(fixture.mock.calls.lock().await[0], request);

    let fixture = Fixture::new(Protocol::Responses, vec![native_success()], |_| {}).await;
    let response = fixture
        .send_body(serde_json::to_vec(&request).unwrap(), Some("unsupported"))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("Unsupported request content-encoding"));
    assert!(fixture.mock.calls.lock().await.is_empty());
}
#[tokio::test]
async fn context_window_error_becomes_failed_sse_only_for_streaming_request() {
    for streaming in [false, true] {
        let fixture = Fixture::new(
            Protocol::ChatCompletions,
            vec![Reply::json(
                StatusCode::BAD_REQUEST,
                json!({"error":{"code":"context_length_exceeded","message":"too many tokens"}}),
            )],
            |_| {},
        )
        .await;
        let response = fixture.send(input(streaming)).await;
        assert_eq!(
            response.status(),
            if streaming {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let text = response.text().await.unwrap();
        assert_eq!(text.contains("event: response.failed"), streaming);
        assert!(text.contains("context_length_exceeded"));
    }
}
#[tokio::test]
async fn anthropic_2xx_error_is_detected_before_stream_commit() {
    for streaming in [false, true] {
        let fixture=Fixture::new(Protocol::Messages,vec![Reply::json(StatusCode::OK,json!({"type":"error","error":{"type":"overloaded_error","message":"backend overloaded"}}))], |_|{}).await;
        let response = fixture.send(input(streaming)).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("backend overloaded"));
    }
}
#[tokio::test]
async fn request_larger_than_old_two_megabyte_limit_reaches_upstream() {
    let fixture = Fixture::new(Protocol::Responses, vec![native_success()], |_| {}).await;
    let mut request = input(false);
    request["input"] = json!("x".repeat(2 * 1024 * 1024 + 128));
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        fixture.mock.calls.lock().await[0]["input"]
            .as_str()
            .unwrap()
            .len()
            > 2 * 1024 * 1024
    );
}
#[tokio::test]
async fn media_fallback_retries_once_and_respects_disabled_and_exhausted_boundaries() {
    for (enabled, recovery_success, expected_calls) in
        [(true, true, 2), (false, true, 1), (true, false, 2)]
    {
        let failure = Reply::json(
            StatusCode::BAD_REQUEST,
            json!({"error":{"message":"image input is not supported"}}),
        );
        let second = if recovery_success {
            native_success()
        } else {
            failure.clone()
        };
        let fixture = Fixture::new(Protocol::Responses, vec![failure, second], |config| {
            config.compat.rectifier.request_media_fallback = enabled;
            config.compat.rectifier.request_media_heuristic = false;
        })
        .await;
        let request = json!({"model":"compat-test-model","input":[{"role":"user","content":[{"type":"input_text","text":"describe"},{"type":"input_image","image_url":"https://example.com/image.png"}]}]});
        let response = fixture.send(request).await;
        assert_eq!(
            response.status(),
            if enabled && recovery_success {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let calls = fixture.mock.calls.lock().await;
        assert_eq!(calls.len(), expected_calls);
        assert!(calls[0].to_string().contains("input_image"));
        if enabled {
            assert!(!calls[1].to_string().contains("input_image"));
            assert!(calls[1].to_string().contains("[Unsupported Image]"));
        }
    }
}
#[tokio::test]
async fn opaque_state_retry_removes_old_reasoning_but_preserves_current_task() {
    let fixture = Fixture::new(
        Protocol::Responses,
        vec![opaque_error(), native_success()],
        |_| {},
    )
    .await;
    let request = json!({"model":"compat-test-model","input":[{"type":"reasoning","id":"rs_old","encrypted_content":"old-opaque-state","summary":[]},agent_task()]});
    let response = fixture.send(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert!(calls[0].to_string().contains("old-opaque-state"));
    assert!(!calls[1].to_string().contains("old-opaque-state"));
    assert!(calls[1].to_string().contains("current-secret-task"));
}
#[tokio::test]
async fn unreadable_current_task_is_not_deleted_or_retried_without_rectifiable_history() {
    let fixture = Fixture::new(Protocol::Responses, vec![opaque_error()], |_| {}).await;
    let response = fixture
        .send(json!({"model":"compat-test-model","input":[agent_task()]}))
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("unreadable_encrypted_agent_task"));
    assert_eq!(fixture.mock.calls.lock().await.len(), 1);
}
#[tokio::test]
async fn opaque_retry_exhaustion_reports_task_error_without_third_attempt() {
    let fixture = Fixture::new(Protocol::Responses, vec![opaque_error()], |_| {}).await;
    let response=fixture.send(json!({"model":"compat-test-model","input":[{"type":"reasoning","id":"rs_old","encrypted_content":"old-state","summary":[]},agent_task()]})).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("unreadable_encrypted_agent_task"));
    assert_eq!(fixture.mock.calls.lock().await.len(), 2);
}
#[tokio::test]
async fn encrypted_task_stream_rejection_is_precommit_but_output_commits_stream() {
    let failure="event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_encrypted_content\",\"message\":\"Could not decrypt encrypted_content\"}}}\n\n";
    for emitted_output in [false, true] {
        let prefix = if emitted_output {
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n"
        } else {
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n"
        };
        let fixture = Fixture::new(
            Protocol::Responses,
            vec![Reply::new(
                StatusCode::OK,
                Some("text/event-stream"),
                format!("{prefix}{failure}"),
            )],
            |_| {},
        )
        .await;
        let response = fixture
            .send(json!({"model":"compat-test-model","stream":true,"input":[agent_task()]}))
            .await;
        assert_eq!(
            response.status(),
            if emitted_output {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            }
        );
        let text = response.text().await.unwrap();
        assert_eq!(
            text.contains("unreadable_encrypted_agent_task"),
            !emitted_output
        );
        if emitted_output {
            assert!(text.contains("hello"));
            assert!(text.contains("response.failed"));
        }
        assert_eq!(fixture.mock.calls.lock().await.len(), 1);
    }
}

#[tokio::test]
async fn native_compaction_request_and_stream_are_both_adapted() {
    let completion = json!({"type":"response.completed","response":{"id":"resp1","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary of earlier work"}]}]}});
    let fixture = Fixture::new(
        Protocol::Responses,
        vec![Reply::new(
            StatusCode::OK,
            Some("text/event-stream"),
            format!("event: response.completed\ndata: {completion}\n\n"),
        )],
        |_| {},
    )
    .await;
    let response=fixture.send(json!({"model":"compat-test-model","stream":true,"input":[{"role":"user","content":"old context"},{"type":"compaction_trigger"}],"tools":[{"type":"function","name":"shell","parameters":{"type":"object","properties":{}}}]})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = response.text().await.unwrap();
    assert!(text.contains("ccswitch-compaction-v1:"));
    assert!(text.contains("response.completed"));
    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls[0]["tool_choice"], "none");
    assert!(!calls[0].to_string().contains("compaction_trigger"));
    assert!(calls[0]
        .to_string()
        .contains("CONTEXT CHECKPOINT COMPACTION"));
}

#[tokio::test]
async fn gemini_native_json_bridge_uses_native_url_auth_and_request_mapping() {
    let fixture = Fixture::new(
        Protocol::GeminiNative,
        vec![Reply::json(
            StatusCode::OK,
            json!({
                "responseId": "gemini-response-1",
                "modelVersion": "gemini-3-pro",
                "candidates": [{
                    "content": {"role": "model", "parts": [
                        {"text": "Checking the weather."},
                        {"functionCall": {"id": "weather-call", "name": "lookup", "args": {"city": "Tokyo"}},
                         "thoughtSignature": "signature-1"}
                    ]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "cachedContentTokenCount": 2,
                    "totalTokenCount": 15
                }
            }),
        )],
        |config| {
            let origin = config.providers[0]
                .base_url
                .strip_suffix("/v1")
                .unwrap()
                .to_string();
            // Gemini accepts an OpenAI-compatible base path as input; the
            // native protocol resolver must replace it with the REST method.
            config.providers[0].base_url = format!("{origin}/v1beta/openai/chat/completions");
            config.model_mappings.push(ModelMappingRule {
                model_pattern: "client-gemini-*".into(),
                target_model: "models/gemini-3-pro".into(),
            });
            config.routes.push(RouteRule {
                name: "mapped-gemini".into(),
                model_pattern: "models/gemini-*".into(),
                provider: "one".into(),
            });
            config.default_route = None;
        },
    )
    .await;

    let input = json!({
        "model": "client-gemini-alias",
        "instructions": "Use the weather tool when needed.",
        "temperature": 0.2,
        "top_p": 0.7,
        "max_output_tokens": 1024,
        "reasoning": {"effort": "high"},
        "tools": [{
            "type": "function",
            "name": "lookup",
            "description": "Look up a city's weather",
            "parameters": {
                "type": "object",
                "properties": {"city": {"type": "string"}},
                "required": ["city"]
            }
        }],
        "tool_choice": {"type": "function", "name": "lookup"},
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "What is the weather in Tokyo?"},
                {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                {"type": "input_file", "filename": "report.pdf", "file_data": "data:application/pdf;base64,cGRm"}
            ]
        }]
    });
    let response = fixture
        .send_with_headers_and_query(
            input,
            Some("alt=client-value&trace=client"),
            &[
                ("authorization", "Bearer client-authorization"),
                ("x-api-key", "client-anthropic-key"),
                ("x-goog-api-key", "client-google-key"),
            ],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["usage"]["input_tokens"], 10);
    assert_eq!(response["usage"]["output_tokens"], 5);
    assert_eq!(response["usage"]["total_tokens"], 15);
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        2
    );
    let output = response["output"].as_array().unwrap();
    assert!(output
        .iter()
        .any(|item| item["content"][0]["text"] == "Checking the weather."));
    assert!(output.iter().any(|item| {
        item["type"] == "function_call"
            && item["name"] == "lookup"
            && item["call_id"] == "weather-call"
    }));

    let calls = fixture.mock.calls.lock().await;
    let sent = &calls[0];
    assert_eq!(
        sent["systemInstruction"]["parts"][0]["text"],
        "Use the weather tool when needed."
    );
    assert_eq!(
        sent["contents"][0]["parts"][0]["text"],
        "What is the weather in Tokyo?"
    );
    assert_eq!(
        sent["contents"][0]["parts"][1]["inlineData"]["mimeType"],
        "image/png"
    );
    assert_eq!(
        sent["contents"][0]["parts"][1]["inlineData"]["data"],
        "aGVsbG8="
    );
    assert_eq!(
        sent["contents"][0]["parts"][2]["inlineData"]["mimeType"],
        "application/pdf"
    );
    assert_eq!(
        sent["contents"][0]["parts"][2]["inlineData"]["data"],
        "cGRm"
    );
    assert_eq!(sent["generationConfig"]["maxOutputTokens"], 1024);
    assert_eq!(sent["generationConfig"]["temperature"], 0.2);
    assert_eq!(sent["generationConfig"]["topP"], 0.7);
    assert_eq!(
        sent["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "HIGH"
    );
    assert_eq!(
        sent["tools"][0]["functionDeclarations"][0]["name"],
        "lookup"
    );
    assert_eq!(sent["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    assert_eq!(
        sent["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
        json!(["lookup"])
    );
    drop(calls);

    let requests = fixture.mock.requests.lock().await;
    assert_eq!(
        requests[0].uri,
        "/v1beta/models/gemini-3-pro:generateContent"
    );
    assert_eq!(
        requests[0].headers["x-goog-api-key"],
        "test-key-not-for-output"
    );
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!requests[0].headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn gemini_native_stream_uses_alt_sse_and_completes_responses_events() {
    let chunks = concat!(
        "data: {\"responseId\":\"stream-1\",\"modelVersion\":\"gemini-2.5-flash\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":5}}\n\n",
        "data: {\"responseId\":\"stream-1\",\"modelVersion\":\"gemini-2.5-flash\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":5}}\n\n"
    );
    let fixture = Fixture::new(
        Protocol::GeminiNative,
        vec![Reply::new(
            StatusCode::OK,
            Some("text/event-stream"),
            chunks,
        )],
        |_| {},
    )
    .await;
    let response = fixture
        .send_with_headers_and_query(
            json!({"model":"gemini-2.5-flash","stream":true,"input":"hello"}),
            Some("alt=json&trace=client"),
            &[],
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let body = response.text().await.unwrap();
    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("\"delta\":\"hello\""));
    assert!(body.contains("event: response.completed"));
    assert!(!body.contains("event: response.incomplete"));
    assert_eq!(body.matches("event: response.output_text.delta").count(), 1);

    let calls = fixture.mock.calls.lock().await;
    assert!(calls[0].get("stream").is_none());
    drop(calls);
    let requests = fixture.mock.requests.lock().await;
    assert_eq!(
        requests[0].uri,
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
}

#[tokio::test]
async fn gemini_native_replays_tool_signatures_with_fingerprint_sessions() {
    let fixture = Fixture::new(
        Protocol::GeminiNative,
        vec![
            Reply::json(
                StatusCode::OK,
                json!({
                    "responseId": "tool-1",
                    "modelVersion": "gemini-3-pro",
                    "candidates": [{
                        "content": {"parts": [{
                            "functionCall": {"name": "lookup", "args": {"city": "Tokyo"}},
                            "thoughtSignature": "signature-replay"
                        }]},
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 8}
                }),
            ),
            Reply::json(
                StatusCode::OK,
                json!({
                    "responseId": "tool-2",
                    "modelVersion": "gemini-3-pro",
                    "candidates": [{
                        "content": {"parts": [{"text": "It is sunny."}]},
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": {"promptTokenCount": 8, "totalTokenCount": 12}
                }),
            ),
        ],
        |_| {},
    )
    .await;
    let user_message = json!({
        "role": "user",
        "content": [{"type": "input_text", "text": "Find the weather in Tokyo."}]
    });
    let tools = json!([{
        "type": "function",
        "name": "lookup",
        "description": "Look up a city's weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
    }]);
    let first = fixture
        .send(json!({
            "model": "gemini-3-pro",
            "input": [user_message.clone()],
            "tools": tools.clone()
        }))
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value = first.json().await.unwrap();
    let tool_call = first["output"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    let client_call_id = tool_call["call_id"].as_str().unwrap().to_string();
    assert!(client_call_id.starts_with("gemini_synth_"));

    let second = fixture
        .send(json!({
            "model": "gemini-3-pro",
            "input": [
                user_message,
                {
                    "type": "function_call",
                    "call_id": client_call_id.clone(),
                    "name": "lookup",
                    "arguments": "{\"city\":\"Tokyo\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": client_call_id,
                    "output": "sunny"
                }
            ],
            "tools": tools
        }))
        .await;
    assert_eq!(second.status(), StatusCode::OK);

    let calls = fixture.mock.calls.lock().await;
    let contents = calls[1]["contents"].as_array().unwrap();
    let assistant_call = contents
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .find(|part| part.get("functionCall").is_some())
        .unwrap();
    assert_eq!(assistant_call["thoughtSignature"], "signature-replay");
    assert!(assistant_call["functionCall"].get("id").is_none());
    let function_response = contents
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .find(|part| part.get("functionResponse").is_some())
        .unwrap();
    assert_eq!(function_response["functionResponse"]["name"], "lookup");
    assert!(function_response["functionResponse"].get("id").is_none());
}

#[tokio::test]
async fn gemini_native_incomplete_tool_call_is_not_committed() {
    let fixture = Fixture::new(
        Protocol::GeminiNative,
        vec![Reply::json(
            StatusCode::OK,
            json!({
                "responseId": "truncated-tool",
                "modelVersion": "gemini-2.5-pro",
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"name": "lookup", "args": {"city": "Tokyo"}}
                    }]},
                    "finishReason": "MAX_TOKENS"
                }],
                "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 8}
            }),
        )],
        |_| {},
    )
    .await;
    let response = fixture
        .send(json!({
            "model": "gemini-2.5-pro",
            "input": "Find the weather in Tokyo.",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }]
        }))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().await.unwrap();
    assert_eq!(response["status"], "incomplete");
    assert_eq!(response["output"], json!([]));
    assert_eq!(
        response["incomplete_details"]["reason"],
        "max_output_tokens"
    );
}

#[tokio::test]
async fn gemini_native_rejects_remote_image_and_document_sources_before_forwarding() {
    for media in [
        json!({"type":"input_image","image_url":"https://example.com/image.png"}),
        json!({"type":"input_file","file_url":"https://example.com/report.pdf"}),
    ] {
        let fixture = Fixture::new(
            Protocol::GeminiNative,
            vec![Reply::json(StatusCode::OK, json!({}))],
            |_| {},
        )
        .await;
        let response = fixture
            .send(json!({
                "model": "gemini-2.5-flash",
                "input": [{"role":"user","content":[media]}]
            }))
            .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("only supports base64"));
        assert!(fixture.mock.calls.lock().await.is_empty());
    }
}

#[tokio::test]
async fn gemini_native_errors_do_not_echo_the_provider_key() {
    for (reply, expected_status) in [
        (
            Reply::json(
                StatusCode::BAD_GATEWAY,
                json!({"error":{"message":"upstream diagnostic test-key-not-for-output"}}),
            ),
            StatusCode::BAD_GATEWAY,
        ),
        (
            Reply::json(
                StatusCode::OK,
                json!({"error":{"status":"INVALID_ARGUMENT","message":"test-key-not-for-output"}}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let fixture = Fixture::new(Protocol::GeminiNative, vec![reply], |_| {}).await;
        let response = fixture.send(input(false)).await;
        assert_eq!(response.status(), expected_status);
        let body = response.text().await.unwrap();
        assert!(!body.contains("test-key-not-for-output"));
    }
}

#[tokio::test]
async fn anthropic_hosted_web_search_replays_raw_blocks_in_the_next_turn() {
    let fixture = Fixture::new(
        Protocol::Messages,
        vec![
            Reply::json(
                StatusCode::OK,
                json!({
                    "id":"msg_search",
                    "type":"message",
                    "role":"assistant",
                    "model":"claude-test",
                    "content":[
                        {"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{"query":"current weather"},"encrypted_content":"use-opaque"},
                        {"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"Weather","url":"https://example.com"}],"encrypted_content":"result-opaque"},
                        {"type":"text","text":"It is sunny."}
                    ],
                    "stop_reason":"end_turn",
                    "usage":{"input_tokens":5,"output_tokens":3,"server_tool_use":{"web_search_requests":1}}
                }),
            ),
            Reply::json(
                StatusCode::OK,
                json!({
                    "id":"msg_followup",
                    "type":"message",
                    "role":"assistant",
                    "model":"claude-test",
                    "content":[{"type":"text","text":"The source says it is sunny."}],
                    "stop_reason":"end_turn",
                    "usage":{"input_tokens":8,"output_tokens":5}
                }),
            ),
        ],
        |_| {},
    )
    .await;

    let first = fixture
        .send(json!({
            "model":"claude-test",
            "stream":false,
            "input":"Find the current weather.",
            "tools":[{"type":"web_search"}],
            "tool_choice":"required"
        }))
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["output"][0]["type"], "web_search_call");
    assert_eq!(first["output"][0]["id"], "ws_srvtoolu_1");
    assert_eq!(first["output"][0]["action"]["query"], "current weather");
    assert!(first["usage"].get("server_tool_use").is_none());

    let second = fixture
        .send(json!({
            "model":"claude-test",
            "stream":false,
            "previous_response_id":first["id"],
            "input":[
                {"type":"message","role":"user","content":"Find the current weather."},
                {"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed","action":{"type":"search","query":"current weather"},"encrypted_content":"codex-opaque"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"It is sunny."}]},
                {"type":"message","role":"user","content":"Summarize the source."}
            ],
            "tools":[{"type":"web_search"}]
        }))
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second: Value = second.json().await.unwrap();
    assert_eq!(
        second["output"][0]["content"][0]["text"],
        "The source says it is sunny."
    );

    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["tools"][0]["type"], "web_search_20250305");
    assert!(calls[0].get("tool_choice").is_none());
    let assistant = calls[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"][0]["type"], "server_tool_use");
    assert_eq!(assistant["content"][0]["encrypted_content"], "use-opaque");
    assert_eq!(assistant["content"][1]["type"], "web_search_tool_result");
    assert_eq!(
        assistant["content"][1]["encrypted_content"],
        "result-opaque"
    );
    assert_eq!(assistant["content"][2]["text"], "It is sunny.");
}

#[tokio::test]
async fn gemini_native_web_search_maps_grounding_queries_and_skips_history_replay() {
    let fixture = Fixture::new(
        Protocol::GeminiNative,
        vec![
            Reply::json(
                StatusCode::OK,
                json!({
                    "responseId": "gem_resp_1",
                    "modelVersion": "gemini-3-pro",
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [{"text": "Rust 1.90 is out."}]
                        },
                        "finishReason": "STOP",
                        "groundingMetadata": {
                            "webSearchQueries": ["rust latest release"]
                        }
                    }],
                    "usageMetadata": {"promptTokenCount": 6, "candidatesTokenCount": 4, "totalTokenCount": 10}
                }),
            ),
            Reply::sse(vec![
                "data: {\"responseId\":\"gem_resp_2\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Details: \"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"totalTokenCount\":12}}\n\n",
                "data: {\"responseId\":\"gem_resp_2\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"Details: edition 2024.\"}]},\"finishReason\":\"STOP\",\"groundingMetadata\":{\"webSearchQueries\":[\"rust 1.90 release notes\",\"rust 1.90 release notes\"]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":6,\"totalTokenCount\":16}}\n\n",
            ]),
        ],
        |_| {},
    )
    .await;

    let first = fixture
        .send(json!({
            "model": "gemini-3-pro",
            "stream": false,
            "input": "What is the latest Rust release?",
            "tools": [{"type": "web_search", "external_web_access": true}]
        }))
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first: Value = first.json().await.unwrap();
    assert_eq!(first["output"][0]["type"], "web_search_call");
    assert_eq!(first["output"][0]["id"], "ws_gem_resp_1_search_0");
    assert_eq!(first["output"][0]["status"], "completed");
    assert_eq!(
        first["output"][0]["action"],
        json!({"type": "search", "query": "rust latest release"})
    );
    assert_eq!(first["output"][1]["type"], "message");
    assert_eq!(
        first["output"][1]["content"][0]["text"],
        "Rust 1.90 is out."
    );

    let second = fixture
        .send(json!({
            "model": "gemini-3-pro",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "What is the latest Rust release?"},
                {"type": "web_search_call", "id": "ws_gem_resp_1_search_0", "status": "completed", "action": {"type": "search", "query": "rust latest release"}},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Rust 1.90 is out."}]},
                {"type": "message", "role": "user", "content": "Check the release notes."}
            ],
            "tools": [{"type": "web_search"}]
        }))
        .await;
    assert_eq!(second.status(), StatusCode::OK);
    let stream_text = second.text().await.unwrap();
    assert!(stream_text.contains("\"type\":\"web_search_call\""));
    assert!(stream_text.contains("\"id\":\"ws_gem_resp_2_search_0\""));
    assert!(stream_text.contains("\"query\":\"rust 1.90 release notes\""));
    assert!(stream_text.contains("Details: edition 2024."));

    let calls = fixture.mock.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["tools"], json!([{"googleSearch": {}}]));
    assert!(calls[0].get("toolConfig").is_none());
    assert_eq!(calls[1]["tools"], json!([{"googleSearch": {}}]));
    assert_eq!(calls[1]["contents"].as_array().unwrap().len(), 3);
}
