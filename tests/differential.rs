//! Compare production conversions with separately vendored, pinned upstream code.
//! This is a conversion oracle, not an end-to-end oracle for forwarder wiring.
use bytes::Bytes;
use cc_switch_lite::proxy::providers as actual;
use cc_switch_reference::proxy::providers as reference;
use futures::{stream, Stream, StreamExt};
use serde_json::{json, Value};

fn outcome<T: std::fmt::Display>(result: Result<Value, T>) -> Result<Value, String> {
    result.map_err(|error| error.to_string())
}

#[test]
fn requests_match_pinned_upstream() {
    let requests = [
        json!({"model":"gpt-5","input":"hello","stream":true,"reasoning":{"effort":"high"}}),
        json!({"model":"claude-opus-4-6","instructions":"be useful","input":[{"role":"user","content":[{"type":"input_text","text":"你好"},{"type":"input_image","image_url":"data:image/png;base64,YQ=="}]}]}),
        json!({"model":"kimi-k2.5","input":[{"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"z\":2,\"a\":1}"},{"type":"function_call_output","call_id":"call_1","output":"sunny"}],"tools":[{"type":"function","name":"weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]}),
        json!({"model":"gemini-3-pro","input":[{"role":"assistant","content":"<think>reason</think>answer"},{"role":"user","content":"continue"}],"reasoning":{"effort":"none"}}),
        json!({"model":"claude-mythos-5","max_output_tokens":16000,"reasoning":{"effort":"high"},"input":"think"}),
        json!({"model":"gpt-5","input":[{"type":"function_call_output","call_id":"missing","output":"orphan"}]}),
        json!({"model":"gpt-5","input":"hi","tools":[{"type":"namespace","name":"functions","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}]}),
        json!({"model":"gpt-5","input":"hi","tools":[{"type":"custom","name":"patch","format":{"type":"text"}}]}),
    ];
    for request in requests {
        assert_eq!(
            outcome(actual::transform_codex_chat::responses_to_chat_completions(
                request.clone()
            )),
            outcome(
                reference::transform_codex_chat::responses_to_chat_completions(request.clone())
            ),
            "Chat request {request}"
        );
        assert_eq!(
            outcome(
                actual::transform_codex_anthropic::responses_request_to_anthropic(
                    request.clone(),
                    8192
                )
            ),
            outcome(
                reference::transform_codex_anthropic::responses_request_to_anthropic(
                    request.clone(),
                    8192
                )
            ),
            "Messages request {request}"
        );
    }
}

#[test]
fn responses_and_errors_match_pinned_upstream() {
    for response in [
        json!({"id":"chatcmpl-fixed","model":"gpt-5","created":42,"choices":[{"message":{"role":"assistant","content":"你好","reasoning_content":"reason"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":3}}),
        json!({"id":"fixed","choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"b\":2,\"a\":1}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[]}),
    ] {
        assert_eq!(
            outcome(actual::transform_codex_chat::chat_completion_to_response(
                response.clone()
            )),
            outcome(reference::transform_codex_chat::chat_completion_to_response(response))
        );
    }
    for response in [
        json!({"id":"msg_fixed","model":"claude","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":"signed"},{"type":"text","text":"你好"},{"type":"tool_use","id":"toolu_1","name":"read","input":{"b":2,"a":1}}],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":5}}),
        json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}),
    ] {
        assert_eq!(
            outcome(
                actual::transform_codex_anthropic::anthropic_response_to_responses(
                    response.clone()
                )
            ),
            outcome(
                reference::transform_codex_anthropic::anthropic_response_to_responses(response)
            )
        );
    }
    for error in [
        json!({"error":{"message":"context exceeded","code":"context_length_exceeded"}}),
        json!({"message":"busy"}),
        Value::Null,
    ] {
        assert_eq!(
            actual::transform_codex_chat::chat_error_to_response_error(Some(&error)),
            reference::transform_codex_chat::chat_error_to_response_error(Some(&error))
        );
    }
}

async fn collect(output: impl Stream<Item = Result<Bytes, std::io::Error>>) -> Vec<u8> {
    futures::pin_mut!(output);
    let mut bytes = Vec::new();
    while let Some(chunk) = output.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    bytes
}

#[tokio::test]
async fn fragmented_sse_matches_pinned_upstream_byte_for_byte() {
    let chat = concat!(
        "data: {\"id\":\"fixed\",\"model\":\"gpt-5\",\"choices\":[{\"delta\":{\"reasoning_content\":\"reason\"}}]}\r\n\n",
        "data: {\"id\":\"fixed\",\"choices\":[{\"delta\":{\"content\":\"你好\"}}]}\n\n",
        "data: {\"id\":\"fixed\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]"
    );
    let messages = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fixed\",\"model\":\"claude\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":3}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}"
    );
    for chunk_size in [1, 7, 4096] {
        let chunks = |body: &str| {
            body.as_bytes()
                .chunks(chunk_size)
                .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            collect(
                actual::streaming_codex_chat::create_responses_sse_stream_from_chat(stream::iter(
                    chunks(chat)
                ))
            )
            .await,
            collect(
                reference::streaming_codex_chat::create_responses_sse_stream_from_chat(
                    stream::iter(chunks(chat))
                )
            )
            .await
        );
        assert_eq!(
            collect(
                actual::streaming_codex_anthropic::create_responses_sse_stream_from_anthropic(
                    stream::iter(chunks(messages))
                )
            )
            .await,
            collect(
                reference::streaming_codex_anthropic::create_responses_sse_stream_from_anthropic(
                    stream::iter(chunks(messages))
                )
            )
            .await
        );
        assert_eq!(
            outcome(actual::transform_codex_anthropic::anthropic_sse_to_message_value(messages)),
            outcome(reference::transform_codex_anthropic::anthropic_sse_to_message_value(messages))
        );
    }
}

#[tokio::test]
async fn request_pipeline_matches_independent_upstream_stages() {
    use cc_switch_lite::{
        compat::{request, CompatConfig},
        config::{Protocol, Provider},
    };
    use cc_switch_reference::request_oracle::{request_body, Facts};
    let fixtures = [
        (
            Protocol::ChatCompletions,
            "chat",
            "https://api.moonshot.cn/v1",
            json!({"model":"kimi-k2.5","input":"hello","_private":"must disappear","tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{"arg":{"$ref":"#/$defs/Value","description":"keep me"}},"$defs":{"Value":{"type":"string"}}}}]}),
        ),
        (
            Protocol::ChatCompletions,
            "chat",
            "https://example.org/v1",
            json!({"model":"gemini-3-pro","input":[{"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":"ok"}]}),
        ),
        (
            Protocol::Messages,
            "messages",
            "https://example.org/v1",
            json!({"model":"claude-opus-4-6[1m]","instructions":"system","input":"hello","tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]}),
        ),
        (
            Protocol::Responses,
            "responses",
            "https://example.org/v1",
            json!({"model":"gpt-5","input":"hello","_trace":"secret","metadata":{"_internal":true,"public":"yes"}}),
        ),
    ];
    for (protocol, wire, base_url, body) in fixtures {
        let provider = Provider {
            id: "oracle".into(),
            name: "Oracle".into(),
            protocol,
            base_url: base_url.into(),
            api_key: "test-key".into(),
            compat: Default::default(),
        };
        let actual = request::prepare(
            body.clone(),
            &provider,
            &Default::default(),
            &CompatConfig::default(),
            &actual::codex_chat_history::CodexChatHistoryStore::default(),
        )
        .await
        .unwrap();
        let expected = request_body(
            body,
            Facts {
                protocol: wire,
                base_url,
                cache_injection: true,
                ..Default::default()
            },
            &reference::codex_chat_history::CodexChatHistoryStore::default(),
        )
        .await
        .unwrap();
        assert_eq!(actual.body, expected, "{wire} at {base_url}");
    }
}

fn synthetic_sse(body: &str) -> reqwest::Response {
    let chunks = body
        .as_bytes()
        .chunks(1)
        .map(|b| Ok::<_, std::io::Error>(Bytes::copy_from_slice(b)))
        .collect::<Vec<_>>();
    axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(reqwest::Body::wrap_stream(stream::iter(chunks)))
        .unwrap()
        .into()
}

fn encrypted_task() -> Value {
    json!({"model":"gpt-5","stream":true,"input":[{"type":"agent_message","author":"/root","recipient":"/root/worker","content":[{"type":"input_text","text":"NEW_TASK"},{"type":"encrypted_content","encrypted_content":"opaque-task"}]}]})
}

#[tokio::test]
async fn encrypted_task_rejection_is_detected_before_stream_commit() {
    let body = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_encrypted_content\",\"message\":\"cannot decrypt\"}}}\n\n"
    );
    let result = cc_switch_lite::compat::retry::hold_encrypted_agent_task_stream(
        synthetic_sse(body),
        &encrypted_task(),
    )
    .await;
    assert!(matches!(
        result,
        Err(cc_switch_lite::proxy::error::ProxyError::UpstreamError { status: 400, .. })
    ));
}

#[tokio::test]
async fn encrypted_task_stream_commits_productive_output_and_replays_exact_bytes() {
    let body = concat!(
        ": heartbeat\n\n",
        "event: response.created\ndata: {\"type\":\"response.created\"}\r\n\r\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\n\n",
        "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"invalid_encrypted_content\"}}}\n\n"
    );
    let response = cc_switch_lite::compat::retry::hold_encrypted_agent_task_stream(
        synthetic_sse(body),
        &encrypted_task(),
    )
    .await
    .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.bytes().await.unwrap().as_ref(), body.as_bytes());
}

#[tokio::test]
async fn url_and_headers_match_pinned_forwarder_header_loop() {
    use axum::http::{HeaderMap, HeaderValue};
    use cc_switch_lite::{
        compat::{request, CompatConfig},
        config::{Protocol, Provider},
    };
    use cc_switch_reference::header_oracle::{request_headers, HeaderFacts};
    let cases = [
        (
            Protocol::Responses,
            "responses",
            "https://example.org",
            false,
            false,
            None,
            json!({"model":"gpt-5","input":"hi"}),
        ),
        (
            Protocol::ChatCompletions,
            "chat",
            "https://example.org/custom",
            false,
            false,
            None,
            json!({"model":"gpt-5","input":"hi","stream":true}),
        ),
        (
            Protocol::ChatCompletions,
            "chat",
            "https://example.org/v1/chat/completions?api-version=preview",
            false,
            false,
            None,
            json!({"model":"gpt-5","input":"hi"}),
        ),
        (
            Protocol::Messages,
            "messages",
            "https://example.org/v1/messages?beta=true",
            false,
            false,
            None,
            json!({"model":"claude-opus-4-6[1m]","input":"hi"}),
        ),
        (
            Protocol::Messages,
            "messages",
            "https://example.org/v1",
            false,
            true,
            None,
            json!({"model":"claude-opus-4-6[1m]","input":"hi"}),
        ),
        (
            Protocol::Responses,
            "responses",
            "https://example.org/special?x=1",
            true,
            false,
            Some("custom-agent/7"),
            json!({"model":"gpt-5","input":"hi"}),
        ),
        (
            Protocol::ChatCompletions,
            "chat",
            "https://opencode.ai/zen/v1",
            false,
            false,
            None,
            json!({"model":"gpt-5","input":"hi","metadata":{"session_id":"short-id"}}),
        ),
        (
            Protocol::ChatCompletions,
            "chat",
            "https://opencode.ai/zen/v1",
            false,
            false,
            Some("custom-agent/7"),
            json!({"model":"gpt-5","input":"hi","metadata":{"user_id":"user_test_session_fallback-id"}}),
        ),
        (
            Protocol::Responses,
            "responses",
            "https://opencode.ai/zen/v1",
            false,
            false,
            None,
            json!({"model":"gpt-5","input":"hi"}),
        ),
    ];
    for (protocol, wire, base_url, full_url, impersonate, custom_user_agent, body) in cases {
        for supplied_identity in [false, true] {
            let mut headers = HeaderMap::new();
            for (name, value) in [
                ("authorization", "Bearer client-key"),
                ("x-api-key", "client-key"),
                ("host", "localhost:15721"),
                ("accept-encoding", "gzip"),
                ("anthropic-beta", "client-beta"),
                ("anthropic-version", "2000-01-01"),
                ("x-forwarded-host", "untrusted"),
                ("x-codex-beta-features", "test-feature"),
                ("x-openai-internal", "private"),
                ("accept", "text/event-stream"),
            ] {
                headers.insert(
                    axum::http::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                );
            }
            if supplied_identity {
                headers.insert(
                    "session_id",
                    HeaderValue::from_static("00000000-0000-0000-0000-000000000001"),
                );
                headers.insert("user-agent", HeaderValue::from_static("codex/test"));
            }
            let mut provider = Provider {
                id: "oracle".into(),
                name: "Oracle".into(),
                protocol,
                base_url: base_url.into(),
                api_key: "provider-secret".into(),
                compat: Default::default(),
            };
            provider.compat.meta.is_full_url = Some(full_url);
            provider.compat.meta.impersonate_claude_code = Some(impersonate);
            provider.compat.meta.custom_user_agent = custom_user_agent.map(str::to_owned);
            let actual = request::prepare(
                body.clone(),
                &provider,
                &headers,
                &CompatConfig::default(),
                &actual::codex_chat_history::CodexChatHistoryStore::default(),
            )
            .await
            .unwrap();
            let (url, mut expected) = request_headers(
                HeaderFacts {
                    protocol: wire,
                    base_url,
                    api_key: "provider-secret",
                    full_url,
                    impersonate,
                    custom_user_agent,
                    version: env!("CARGO_PKG_VERSION"),
                },
                &headers,
                &body,
                &actual.body,
            );
            // No client identity/conversation fingerprint: only generated UUIDs vary.
            if let (Some(a), Some(b)) = (
                actual.headers.get("x-opencode-session"),
                expected.get("x-opencode-session"),
            ) {
                if uuid::Uuid::parse_str(a.to_str().unwrap()).is_ok()
                    && uuid::Uuid::parse_str(b.to_str().unwrap()).is_ok()
                    && !supplied_identity
                {
                    expected.insert("x-opencode-session", a.clone());
                }
            }
            assert_eq!(actual.url, url, "URL {wire} {base_url}");
            assert_eq!(
                actual.headers, expected,
                "headers {wire} {base_url}, identity={supplied_identity}"
            );
        }
    }
}
