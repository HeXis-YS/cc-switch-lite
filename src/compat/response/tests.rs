use super::*;
use serde_json::json;
fn response(status: StatusCode, kind: Option<&str>, body: impl Into<Bytes>) -> reqwest::Response {
    let mut builder = axum::http::Response::builder().status(status);
    if let Some(kind) = kind {
        builder = builder.header(header::CONTENT_TYPE, kind);
    }
    builder.body(body.into()).unwrap().into()
}
fn options(streaming: bool) -> ResponseOptions {
    ResponseOptions {
        model: "test".into(),
        streaming,
        late_arguments_repair: false,
        native_compaction: false,
        xai_restore: None,
    }
}
async fn run(response: reqwest::Response, protocol: Protocol, streaming: bool) -> Response {
    handle(
        response,
        protocol,
        options(streaming),
        Default::default(),
        Arc::default(),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn unmarked_chat_sse_is_aggregated_for_nonstreaming_client() {
    let source="data: {\"id\":\"c1\",\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\n";
    let result = run(
        response(StatusCode::OK, None, source),
        Protocol::ChatCompletions,
        false,
    )
    .await;
    assert_eq!(result.headers()[header::CONTENT_TYPE], "application/json");
    let body = axum::body::to_bytes(result.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["output"][0]["content"][0]["text"], "hello");
}
#[tokio::test]
async fn context_error_uses_failed_sse_only_for_streaming_bad_request() {
    let value = json!({"error":{"code":"context_length_exceeded","type":"invalid_request_error","message":"context too long"}});
    for (streaming, status, expected) in [
        (true, StatusCode::BAD_REQUEST, StatusCode::OK),
        (false, StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST),
        (
            true,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS,
        ),
    ] {
        let result = run(
            response(status, Some("application/json"), value.to_string()),
            Protocol::ChatCompletions,
            streaming,
        )
        .await;
        assert_eq!(result.status(), expected);
        let body = axum::body::to_bytes(result.into_body(), usize::MAX)
            .await
            .unwrap();
        if expected == StatusCode::OK {
            assert!(String::from_utf8_lossy(&body).contains("response.failed"));
        }
    }
}
#[tokio::test]
async fn non_json_error_retains_diagnostic_and_status() {
    let result = run(
        response(
            StatusCode::BAD_GATEWAY,
            Some("text/plain"),
            "upstream unavailable",
        ),
        Protocol::Messages,
        false,
    )
    .await;
    assert_eq!(result.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(result.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&body).contains("upstream unavailable"));
}
#[tokio::test]
async fn anthropic_explicit_json_streaming_response_synthesizes_sse() {
    let value = json!({"id":"msg_1","type":"message","role":"assistant","model":"test","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}});
    let result = run(
        response(
            StatusCode::OK,
            Some("Application/JSON; charset=utf-8"),
            value.to_string(),
        ),
        Protocol::Messages,
        true,
    )
    .await;
    let body = axum::body::to_bytes(result.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("response.completed"));
    assert!(text.contains("hello"));
}
#[tokio::test]
async fn gzip_body_is_decoded_and_entity_headers_removed() {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(b"{\"ok\":true}").unwrap();
    let body = encoder.finish().unwrap();
    let response = axum::http::Response::builder()
        .header("content-encoding", "gzip")
        .header("content-length", body.len())
        .body(Bytes::from(body))
        .unwrap()
        .into();
    let (headers, _, body) = decode_response(response).await.unwrap();
    assert_eq!(&body[..], b"{\"ok\":true}");
    assert!(!headers.contains_key("content-encoding"));
    assert!(!headers.contains_key("content-length"));
}
