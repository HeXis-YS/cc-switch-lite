use axum::http::{HeaderMap, HeaderValue};
use cc_switch_lite::{
    compat::{
        request::{prepare, prepare_with_query, ProviderCompat},
        CompatConfig,
    },
    config::{Protocol, Provider},
    proxy::providers::codex_chat_history::CodexChatHistoryStore,
};
use serde_json::json;
fn provider(protocol: Protocol, url: &str) -> Provider {
    Provider {
        id: "p".into(),
        name: "p".into(),
        protocol,
        base_url: url.into(),
        api_key: "test-key".into(),
        compat: ProviderCompat::default(),
    }
}

#[tokio::test]
async fn chat_cache_uses_real_prefixed_session_and_reasoning_platform() {
    let mut p = provider(Protocol::ChatCompletions, "https://openrouter.ai/api/v1");
    p.compat.meta.prompt_cache_routing = Some("enabled".into());
    let mut headers = HeaderMap::new();
    headers.insert(
        "session_id",
        HeaderValue::from_static("12345678-1234-1234-1234-123456789012"),
    );
    let out = prepare(
        json!({"model":"deepseek-v4","reasoning":{"effort":"xhigh"},"input":"Hi"}),
        &p,
        &headers,
        &CompatConfig::default(),
        &CodexChatHistoryStore::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        out.body["prompt_cache_key"],
        "codex_12345678-1234-1234-1234-123456789012"
    );
    assert_eq!(out.body["reasoning"]["effort"], "xhigh");
    assert!(out.body.get("thinking").is_none());
}

#[tokio::test]
async fn anthropic_default_budget_preserves_explicit_client_limit() {
    let p = provider(Protocol::Messages, "https://example.test/v1");
    for (limit, expected) in [(None, 128000), (Some(1000), 1000)] {
        let mut input = json!({"model":"claude-fable-5-1","input":"Hi"});
        if let Some(limit) = limit {
            input["max_output_tokens"] = json!(limit);
        }
        let out = prepare(
            input,
            &p,
            &HeaderMap::new(),
            &CompatConfig::default(),
            &CodexChatHistoryStore::default(),
        )
        .await
        .unwrap();
        assert_eq!(out.body["max_tokens"], expected);
    }
}

#[tokio::test]
async fn anthropic_configuration_is_applied_before_thinking_and_header_conversion() {
    let mut p = provider(
        Protocol::Messages,
        "https://example.test/custom/v1/messages",
    );
    p.compat.meta.max_output_tokens = Some(16000);
    p.compat.meta.impersonate_claude_code = Some(true);
    let mut h = HeaderMap::new();
    h.insert(
        "anthropic-version",
        HeaderValue::from_static("custom-version"),
    );
    h.insert(
        "session_id",
        HeaderValue::from_static("private-client-identity"),
    );
    h.insert("accept", HeaderValue::from_static("text/event-stream"));
    let out=prepare(json!({"model":"claude-sonnet-4[1M]","input":"Hi","instructions":"Be concise","max_output_tokens":1000,"reasoning":{"effort":"high"}}),&p,&h,&CompatConfig::default(),&CodexChatHistoryStore::default()).await.unwrap();
    assert_eq!(out.url, "https://example.test/custom/v1/messages");
    assert_eq!(out.body["model"], "claude-sonnet-4");
    assert_eq!(out.body["max_tokens"], 16000);
    assert_eq!(
        out.body["system"][0]["text"],
        "You are Claude Code, Anthropic's official CLI for Claude."
    );
    assert!(out.body["system"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .get("cache_control")
        .is_some());
    assert_eq!(
        out.headers["anthropic-beta"],
        "claude-code-20250219,context-1m-2025-08-07"
    );
    assert_eq!(out.headers["anthropic-version"], "2023-06-01");
    assert_eq!(out.headers["accept"], "application/json");
    assert!(!out.headers.contains_key("session_id"));
    assert!(!out.headers.contains_key("authorization"));
    assert_eq!(out.headers["x-api-key"], "test-key");
}

#[tokio::test]
async fn xai_namespace_context_survives_request_flattening() {
    let p = provider(Protocol::Responses, "https://api.x.ai/v1");
    let out=prepare(json!({"model":"grok-4","input":"Hi","tools":[{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}]}],"_private":"drop","prompt_cache_retention":"24h"}),&p,&HeaderMap::new(),&CompatConfig::default(),&CodexChatHistoryStore::default()).await.unwrap();
    assert_eq!(out.body["tools"][0]["type"], "function");
    assert!(!out.late_arguments_repair);
    assert!(out.body.get("_private").is_none());
    assert!(out.body.get("prompt_cache_retention").is_none());
}

#[tokio::test]
async fn full_endpoint_keeps_query_and_uses_request_query() {
    let mut p = provider(Protocol::Responses, "https://example.test/api?version=one");
    p.compat.meta.is_full_url = Some(true);
    let out = prepare_with_query(
        json!({"model":"gpt[1M]","input":"Hi"}),
        &p,
        &HeaderMap::new(),
        &CompatConfig::default(),
        &CodexChatHistoryStore::default(),
        Some("trace=two"),
    )
    .await
    .unwrap();
    assert_eq!(out.url, "https://example.test/api?version=one&trace=two");
    assert_eq!(out.body["model"], "gpt");
}

#[tokio::test]
async fn opencode_uses_native_session_without_changing_existing_user_agent() {
    let p = provider(Protocol::ChatCompletions, "https://opencode.ai/zen/v1");
    let mut h = HeaderMap::new();
    h.insert("session-id", HeaderValue::from_static("real-session"));
    h.insert("user-agent", HeaderValue::from_static("codex-client"));
    let out = prepare(
        json!({"model":"gpt","input":"Hi"}),
        &p,
        &h,
        &CompatConfig::default(),
        &CodexChatHistoryStore::default(),
    )
    .await
    .unwrap();
    assert_eq!(out.headers["x-opencode-session"], "real-session");
    assert_eq!(out.headers["user-agent"], "codex-client");
}
