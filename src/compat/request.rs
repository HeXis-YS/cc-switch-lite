// Copyright (c) 2025 Jason Young. MIT; see ../proxy/providers/LICENSE-cc-switch.
//! Single API-key provider projection of cc-switch's Codex forwarder.
//! Source: 214e75ba7f625c679dd4e61df166ec4463245fe7; see upstream_request.
use super::CompatConfig;
use crate::config::{Protocol, Provider};
use crate::proxy::{
    error::ProxyError,
    providers::{self, codex_chat_history::CodexChatHistoryStore},
};
use axum::http::HeaderMap;
use serde_json::{json, Value};
#[path = "upstream_request/codex.rs"]
mod codex;
#[path = "upstream_request/forwarder.rs"]
mod forwarder;
use forwarder::*;

mod headers;
mod provider;
use headers::prepare_headers;
pub use provider::{upstream_provider, CompatMeta, ProviderCompat};

pub struct Prepared {
    pub body: Value,
    pub headers: HeaderMap,
    pub url: String,
    pub late_arguments_repair: bool,
    pub native_compaction: bool,
    pub request_is_streaming: bool,
    #[allow(dead_code)]
    pub(crate) xai_restore: Option<
        std::collections::HashMap<
            String,
            providers::transform_codex_responses_namespace::NamespacedName,
        >,
    >,
}

pub(crate) struct RequestContext {
    protocol: Protocol,
    session: String,
    session_source: crate::proxy::session::SessionIdSource,
    generated_session: String,
    fingerprint: Option<String>,
    one_m: Option<bool>,
    native_compaction: bool,
    xai_restore: Option<
        std::collections::HashMap<
            String,
            providers::transform_codex_responses_namespace::NamespacedName,
        >,
    >,
    endpoint: String,
    url: String,
    request_is_streaming: bool,
    late_arguments_repair: bool,
    impersonate_claude_code: bool,
    xai_native: bool,
}

impl RequestContext {
    fn client_session(&self) -> Option<&str> {
        (self.session_source != crate::proxy::session::SessionIdSource::Generated)
            .then_some(self.session.as_str())
    }

    pub(crate) fn response_options(
        &self,
        model: &str,
        streaming: bool,
    ) -> crate::compat::response::ResponseOptions {
        crate::compat::response::ResponseOptions {
            model: model.to_owned(),
            streaming,
            late_arguments_repair: self.late_arguments_repair,
            native_compaction: self.native_compaction,
            xai_restore: self.xai_restore.clone(),
        }
    }
}

pub async fn prepare(
    input: Value,
    provider: &Provider,
    headers: &HeaderMap,
    global: &CompatConfig,
    history: &CodexChatHistoryStore,
) -> Result<Prepared, ProxyError> {
    prepare_with_query(input, provider, headers, global, history, None).await
}

pub async fn prepare_with_query(
    input: Value,
    provider: &Provider,
    headers: &HeaderMap,
    global: &CompatConfig,
    history: &CodexChatHistoryStore,
    query: Option<&str>,
) -> Result<Prepared, ProxyError> {
    let mut context = build_context(provider.protocol, provider, headers, &input, query);
    prepare_attempt(&mut context, input, provider, headers, global, history).await
}

pub(crate) fn build_context(
    protocol: Protocol,
    provider: &Provider,
    headers: &HeaderMap,
    input: &Value,
    query: Option<&str>,
) -> RequestContext {
    let normalized_input = normalize_thinking_type(input.clone());
    let upstream = upstream_provider(provider);
    let extracted_session =
        crate::proxy::session::extract_session_id(headers, &normalized_input, "codex");
    let session_source = extracted_session.source;
    let session = extracted_session.session_id;
    let generated_session = if session_source == crate::proxy::session::SessionIdSource::Generated {
        session.clone()
    } else {
        String::new()
    };
    let client_session = (session_source != crate::proxy::session::SessionIdSource::Generated)
        .then_some(session.as_str());
    let fingerprint = if client_session.is_none()
        && !headers.contains_key("x-opencode-session")
        && native_session_header(headers).is_none()
    {
        conversation_fingerprint(&normalized_input)
    } else {
        None
    };
    let xai_native =
        protocol == Protocol::Responses && codex::provider_is_xai_native_responses(&upstream);
    let xai_restore = xai_native.then(|| {
        providers::transform_codex_responses_namespace::namespace_restore_map(&normalized_input)
    });
    let endpoint = effective_endpoint(protocol, query);
    let url = resolve_upstream_url(
        protocol,
        &provider.base_url,
        provider.compat.meta.is_full_url == Some(true),
        query,
    );
    let request_is_streaming = is_streaming_request(&endpoint, &normalized_input, headers);

    RequestContext {
        protocol,
        session,
        session_source,
        generated_session,
        fingerprint,
        one_m: (protocol != Protocol::Messages).then_some(false),
        native_compaction: protocol == Protocol::Responses
            && providers::codex_compaction::is_compaction_request(&normalized_input),
        xai_restore,
        endpoint,
        url,
        request_is_streaming,
        late_arguments_repair: protocol == Protocol::Responses && !xai_native,
        impersonate_claude_code: protocol == Protocol::Messages
            && provider.compat.meta.impersonate_claude_code == Some(true),
        xai_native,
    }
}

pub(crate) async fn prepare_attempt(
    context: &mut RequestContext,
    input: Value,
    provider: &Provider,
    headers: &HeaderMap,
    global: &CompatConfig,
    history: &CodexChatHistoryStore,
) -> Result<Prepared, ProxyError> {
    let mut input = normalize_thinking_type(input);
    let upstream = upstream_provider(provider);
    let session = context.client_session().map(str::to_owned);
    if context.protocol != Protocol::Messages {
        if let Some(model) = input.get("model").and_then(Value::as_str) {
            let stripped = strip_one_m_suffix_for_upstream(model);
            if stripped != model {
                input["model"] = json!(stripped);
            }
        }
    }
    let mut body = match context.protocol {
        Protocol::ChatCompletions => {
            let explicit = input
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .map(str::to_owned);
            history.enrich_request(&mut input).await;
            let reasoning = codex::resolve_codex_chat_reasoning_config(&upstream, &input);
            let mut body =
                providers::transform_codex_chat::responses_to_chat_completions_with_reasoning(
                    input,
                    reasoning.as_ref(),
                )?;
            providers::codex_chat_common::fill_gemini_thought_signature_fallback(&mut body);
            codex::inject_codex_chat_prompt_cache_key(
                &upstream,
                &mut body,
                explicit.as_deref(),
                session.as_deref(),
            );
            body
        }
        Protocol::Messages => {
            if let Some(max) = provider.compat.meta.max_output_tokens.filter(|v| *v > 0) {
                input["max_output_tokens"] = json!(max);
            }
            let mut body = providers::transform_codex_anthropic::responses_request_to_anthropic(
                input, 128000,
            )?;
            let one_m = if let Some(model) = body.get("model").and_then(Value::as_str) {
                let stripped = strip_one_m_suffix_for_upstream(model);
                if stripped != model {
                    body["model"] = json!(stripped);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            context.one_m.get_or_insert(one_m);
            if context.impersonate_claude_code {
                prepend_claude_code_system_prompt(&mut body);
            }
            crate::proxy::cache_injector::inject(
                &mut body,
                &codex_anthropic_cache_config(&global.optimizer),
            );
            body
        }
        Protocol::Responses => {
            providers::codex_compaction::prepare_native_third_party_request(&mut input);
            if context.xai_native {
                providers::transform_codex_responses_namespace::flatten_request_namespaces(
                    &mut input,
                )?;
                providers::transform_codex_responses_xai_sanitize::apply_xai_native_responses_request_compat(&mut input,&provider.id,None,&upstream.settings_config);
            }
            input
        }
    };
    if provider.protocol == Protocol::ChatCompletions
        && providers::transform_codex_chat_moonshot_schema::upstream_requires_ref_sibling_all_of(
            &provider.base_url,
        )
    {
        providers::transform_codex_chat_moonshot_schema::wrap_ref_siblings_in_chat_tools(&mut body);
    }
    if global.rectifier.enabled && global.rectifier.request_media_fallback {
        crate::proxy::media_sanitizer::replace_images_for_text_only_model(
            &mut body,
            &upstream,
            global.rectifier.request_media_heuristic,
        );
    }
    let mut body = prepare_upstream_request_body(body);
    if let Some(overrides) = &provider.compat.meta.local_proxy_request_overrides {
        if apply_local_proxy_body_overrides(&mut body, overrides) {
            body = prepare_upstream_request_body(body);
        }
    }
    let outbound_headers = prepare_headers(
        provider,
        headers,
        &context.url,
        &context.endpoint,
        context.impersonate_claude_code,
        context.one_m.unwrap_or(false),
        session.as_deref(),
        context.fingerprint.as_deref(),
        &context.generated_session,
        context.request_is_streaming,
    )?;
    Ok(Prepared {
        body,
        headers: outbound_headers,
        url: context.url.clone(),
        native_compaction: context.native_compaction,
        request_is_streaming: context.request_is_streaming,
        late_arguments_repair: context.late_arguments_repair,
        xai_restore: context.xai_restore.clone(),
    })
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn provider(protocol: Protocol) -> Provider {
        Provider {
            id: "opencode".into(),
            name: "OpenCode".into(),
            protocol,
            base_url: "https://opencode.ai/v1".into(),
            api_key: "test-key".into(),
            compat: ProviderCompat::default(),
        }
    }

    #[tokio::test]
    async fn retry_reuses_stable_url_and_identity_headers_after_media_rectification() {
        let provider = provider(Protocol::Messages);
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", HeaderValue::from_static("codex-client"));
        let input = json!({
            "model": "claude-test[1m]",
            "stream": true,
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "describe"},
                    {"type": "input_image", "image_url": "https://example.com/image.png"}
                ]
            }]
        });
        let global = CompatConfig::default();
        let history = CodexChatHistoryStore::default();
        let mut context = build_context(
            provider.protocol,
            &provider,
            &headers,
            &input,
            Some("trace=two"),
        );
        let first = prepare_attempt(
            &mut context,
            input.clone(),
            &provider,
            &headers,
            &global,
            &history,
        )
        .await
        .unwrap();
        let error = ProxyError::UpstreamError {
            status: 400,
            body: Some(json!({"error":{"message":"image input is not supported"}}).to_string()),
        };
        let rectified =
            crate::compat::retry::rectify(provider.protocol, &input, &error, &global.rectifier)
                .unwrap()
                .expect("unsupported image input should be rectified");
        let second = prepare_attempt(
            &mut context,
            rectified,
            &provider,
            &headers,
            &global,
            &history,
        )
        .await
        .unwrap();

        assert_ne!(first.body, second.body);
        assert!(first
            .body
            .to_string()
            .contains("https://example.com/image.png"));
        assert!(second.body.to_string().contains("[Unsupported Image]"));
        assert_eq!(first.url, "https://opencode.ai/v1/messages?trace=two");
        assert_eq!(first.url, second.url);
        assert_eq!(first.request_is_streaming, second.request_is_streaming);
        assert_eq!(first.headers, second.headers);
        assert_eq!(first.headers["user-agent"], "codex-client");
        assert_eq!(first.headers["anthropic-beta"], "context-1m-2025-08-07");
        assert!(!first.headers["x-opencode-session"].is_empty());
        assert_eq!(
            first.headers["x-opencode-session"],
            second.headers["x-opencode-session"]
        );
    }

    #[tokio::test]
    async fn retry_reuses_client_session_and_prompt_cache_identity() {
        let provider = Provider {
            id: "kimi".into(),
            name: "Kimi".into(),
            protocol: Protocol::ChatCompletions,
            base_url: "https://api.kimi.com/coding/v1".into(),
            api_key: "test-key".into(),
            compat: ProviderCompat::default(),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "session_id",
            HeaderValue::from_static("client-session-123456789"),
        );
        let input = json!({
            "model": "kimi-for-coding",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "describe"},
                    {"type": "input_image", "image_url": "https://example.com/image.png"}
                ]
            }]
        });
        let global = CompatConfig::default();
        let history = CodexChatHistoryStore::default();
        let mut context = build_context(provider.protocol, &provider, &headers, &input, None);
        let first = prepare_attempt(
            &mut context,
            input.clone(),
            &provider,
            &headers,
            &global,
            &history,
        )
        .await
        .unwrap();
        let retry_input = crate::compat::retry::rectify(
            provider.protocol,
            &input,
            &ProxyError::UpstreamError {
                status: 400,
                body: Some(json!({"error":{"message":"image input is not supported"}}).to_string()),
            },
            &global.rectifier,
        )
        .unwrap()
        .unwrap();
        let second = prepare_attempt(
            &mut context,
            retry_input,
            &provider,
            &headers,
            &global,
            &history,
        )
        .await
        .unwrap();

        assert_ne!(first.body, second.body);
        assert_eq!(
            first.body["prompt_cache_key"],
            "codex_client-session-123456789"
        );
        assert_eq!(
            first.body["prompt_cache_key"],
            second.body["prompt_cache_key"]
        );
        assert_eq!(first.headers, second.headers);
    }
}
