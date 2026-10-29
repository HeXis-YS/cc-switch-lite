//! End-to-end compatibility checks through real proxy and mock HTTP sockets.
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, State},
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
    replies: Arc<Vec<Reply>>,
}
async fn upstream(State(mock): State<Mock>, Json(body): Json<Value>) -> Response {
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
            replies: Arc::new(replies),
        };
        let (url, upstream_task) = start(
            Router::new()
                .route("/v1/responses", post(upstream))
                .route("/v1/chat/completions", post(upstream))
                .route("/v1/messages", post(upstream))
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
            route: Route {
                provider: "one".into(),
            },
            routes: vec![],
            default_route: None,
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
        reqwest::Client::new()
            .post(format!("{}/v1/responses", self.url))
            .json(&input)
            .send()
            .await
            .unwrap()
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
