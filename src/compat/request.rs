// Copyright (c) 2025 Jason Young. MIT; see ../proxy/providers/LICENSE-cc-switch.
//! Single API-key provider projection of cc-switch's Codex forwarder.
//! Source: 214e75ba7f625c679dd4e61df166ec4463245fe7; see upstream_request.
use super::CompatConfig;
use crate::config::{Protocol, Provider};
use crate::proxy::{
    error::ProxyError,
    providers::{
        self,
        codex_chat_history::{CodexChatHistoryStore, WebSearchShadowLookup},
        gemini_shadow::GeminiShadowStore,
        transform_codex_chat::CodexToolContext,
        transform_gemini::AnthropicToolSchemaHints,
    },
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
use headers::{prepare_headers, HeaderSource};
pub use provider::{
    upstream_provider, CompatMeta, GeminiCompat, GeminiThinking, ProviderCompat, WebSearchCompat,
};

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
    pub(crate) gemini_tool_schema_hints: Option<AnthropicToolSchemaHints>,
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
    url: String,
    request_is_streaming: bool,
    late_arguments_repair: bool,
    impersonate_claude_code: bool,
    xai_native: bool,
    gemini_provider_id: Option<String>,
    gemini_session_id: Option<String>,
    gemini_tool_schema_hints: Option<AnthropicToolSchemaHints>,
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
            streaming: streaming
                || (self.protocol == Protocol::GeminiNative && self.request_is_streaming),
            late_arguments_repair: self.late_arguments_repair,
            native_compaction: self.native_compaction,
            xai_restore: self.xai_restore.clone(),
            gemini_provider_id: self.gemini_provider_id.clone(),
            gemini_session_id: self.gemini_session_id.clone(),
            gemini_tool_schema_hints: self.gemini_tool_schema_hints.clone(),
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
    let shadow = GeminiShadowStore::default();
    prepare_attempt(
        &mut context,
        input,
        provider,
        headers,
        global,
        history,
        Some(&shadow),
    )
    .await
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
        if protocol == Protocol::GeminiNative {
            gemini_conversation_fingerprint(&normalized_input)
        } else {
            conversation_fingerprint(&normalized_input)
        }
    } else {
        None
    };
    let xai_native =
        protocol == Protocol::Responses && codex::provider_is_xai_native_responses(&upstream);
    let xai_restore = xai_native.then(|| {
        providers::transform_codex_responses_namespace::namespace_restore_map(&normalized_input)
    });
    let request_is_streaming = if protocol == Protocol::GeminiNative {
        // The client query is ignored for Gemini. Streaming is selected from the
        // Responses body/header, then the canonical endpoint adds only alt=sse.
        is_streaming_request("/generateContent", &normalized_input, headers)
    } else {
        let endpoint = effective_endpoint(protocol, query);
        is_streaming_request(&endpoint, &normalized_input, headers)
    };
    let url = if protocol == Protocol::GeminiNative {
        let model = normalized_input
            .get("model")
            .and_then(Value::as_str)
            .map(strip_one_m_suffix_for_upstream)
            .map(crate::proxy::gemini_url::normalize_gemini_model_id)
            .unwrap_or("");
        let method = if request_is_streaming {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let endpoint = if request_is_streaming {
            format!("/v1beta/models/{model}:{method}?alt=sse")
        } else {
            format!("/v1beta/models/{model}:{method}")
        };
        crate::proxy::gemini_url::resolve_gemini_native_url(
            &provider.base_url,
            &endpoint,
            provider.compat.meta.is_full_url == Some(true),
        )
    } else {
        resolve_upstream_url(
            protocol,
            &provider.base_url,
            provider.compat.meta.is_full_url == Some(true),
            query,
        )
    };
    let opencode_session = headers
        .get("x-opencode-session")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let gemini_session_id = if protocol == Protocol::GeminiNative {
        client_session
            .or(opencode_session)
            .or_else(|| native_session_header(headers))
            .or(fingerprint.as_deref())
            .map(ToOwned::to_owned)
    } else {
        None
    };

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
        url,
        request_is_streaming,
        late_arguments_repair: protocol == Protocol::Responses && !xai_native,
        impersonate_claude_code: protocol == Protocol::Messages
            && provider.compat.meta.impersonate_claude_code == Some(true),
        xai_native,
        gemini_provider_id: (protocol == Protocol::GeminiNative).then(|| provider.id.clone()),
        gemini_session_id,
        gemini_tool_schema_hints: None,
    }
}

pub(crate) async fn prepare_attempt(
    context: &mut RequestContext,
    input: Value,
    provider: &Provider,
    headers: &HeaderMap,
    global: &CompatConfig,
    history: &CodexChatHistoryStore,
    gemini_shadow: Option<&GeminiShadowStore>,
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
            let mut tool_context =
                providers::transform_codex_chat::build_codex_tool_context_from_request_with_web_search(
                    &input,
                    provider.compat.web_search.is_enabled(),
                    Some(provider.id.clone()),
                );
            attach_anthropic_web_search_history(&input, &provider.id, history, &mut tool_context)
                .await;
            let mut body =
                providers::transform_codex_anthropic::responses_request_to_anthropic_with_context(
                    input,
                    128000,
                    &tool_context,
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
        Protocol::GeminiNative => {
            let max_output_tokens_was_set = input
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .is_some_and(|value| value > 0)
                || provider
                    .compat
                    .meta
                    .max_output_tokens
                    .is_some_and(|value| value > 0);
            if let Some(max) = provider.compat.meta.max_output_tokens.filter(|v| *v > 0) {
                input["max_output_tokens"] = json!(max);
            }
            let model = input
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let reasoning_effort = input
                .pointer("/reasoning/effort")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let mut tool_context =
                providers::transform_codex_chat::build_codex_tool_context_from_request_with_web_search(
                    &input,
                    provider.compat.web_search.is_enabled(),
                    Some(provider.id.clone()),
                );
            mark_gemini_web_search_history_unavailable(&input, &mut tool_context);
            let anthropic =
                providers::transform_codex_anthropic::responses_request_to_anthropic_with_context(
                    input.clone(),
                    128000,
                    &tool_context,
                )?;
            let tool_schema_hints =
                providers::transform_gemini::extract_anthropic_tool_schema_hints(&anthropic);
            let mut body =
                providers::transform_gemini::anthropic_to_gemini_with_shadow_and_options(
                    anthropic,
                    gemini_shadow,
                    context.gemini_provider_id.as_deref(),
                    context.gemini_session_id.as_deref(),
                    provider.compat.gemini.thought_signature_replay,
                )?;
            if !max_output_tokens_was_set {
                omit_implicit_gemini_max_output_tokens(&mut body);
            }
            copy_responses_sampling_parameters(&mut body, &input);
            apply_gemini_thinking_level(
                &mut body,
                &model,
                reasoning_effort.as_deref(),
                &provider.compat.gemini,
            )?;
            context.gemini_tool_schema_hints = Some(tool_schema_hints.clone());
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
    let outbound_headers = prepare_headers(HeaderSource {
        provider,
        headers,
        url: &context.url,
        impersonate_claude_code: context.impersonate_claude_code,
        anthropic_one_m: context.one_m.unwrap_or(false),
        session: session.as_deref(),
        fingerprint: context.fingerprint.as_deref(),
        generated_session: &context.generated_session,
        request_is_streaming: context.request_is_streaming,
    })?;
    Ok(Prepared {
        body,
        headers: outbound_headers,
        url: context.url.clone(),
        native_compaction: context.native_compaction,
        request_is_streaming: context.request_is_streaming,
        late_arguments_repair: context.late_arguments_repair,
        xai_restore: context.xai_restore.clone(),
        gemini_tool_schema_hints: context.gemini_tool_schema_hints.clone(),
    })
}

fn omit_implicit_gemini_max_output_tokens(body: &mut Value) {
    // The Anthropic bridge requires max_tokens and supplies 128000 when the
    // Responses request omits max_output_tokens. Gemini's field is optional
    // and model-specific, so that bridge default must not become an upstream
    // limit that may exceed the selected Gemini model's capacity.
    let remove_generation_config = body
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
        .is_some_and(|config| {
            config.remove("maxOutputTokens");
            config.is_empty()
        });
    if remove_generation_config {
        if let Some(object) = body.as_object_mut() {
            object.remove("generationConfig");
        }
    }
}

fn copy_responses_sampling_parameters(gemini_body: &mut Value, responses_body: &Value) {
    for (source, target) in [("temperature", "temperature"), ("top_p", "topP")] {
        if let Some(value) = responses_body.get(source) {
            gemini_body["generationConfig"][target] = value.clone();
        }
    }
}

async fn attach_anthropic_web_search_history(
    input: &Value,
    provider_id: &str,
    history: &CodexChatHistoryStore,
    tool_context: &mut CodexToolContext,
) {
    if !tool_context.accepts_web_search() || tool_context.is_compaction_request() {
        return;
    }
    let Some(items) = input.get("input").and_then(Value::as_array) else {
        return;
    };

    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("web_search_call")
            || item.get("status").and_then(Value::as_str) == Some("incomplete")
        {
            continue;
        }
        let Some(response_item_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            log::warn!("[Codex/Anthropic] Dropping historical web_search_call without an id");
            continue;
        };

        match history
            .lookup_anthropic_web_search_shadow(response_item_id, provider_id)
            .await
        {
            WebSearchShadowLookup::Found(shadow) => {
                tool_context.add_web_search_shadow(response_item_id.to_string(), shadow);
            }
            WebSearchShadowLookup::Missing => {
                tool_context.mark_web_search_shadow_unavailable(response_item_id.to_string());
                log::warn!("[Codex/Anthropic] Dropping historical web_search_call without cached Anthropic blocks");
            }
            WebSearchShadowLookup::ProviderMismatch => {
                tool_context.mark_web_search_shadow_unavailable(response_item_id.to_string());
                log::warn!("[Codex/Anthropic] Dropping historical web_search_call cached for a different Provider");
            }
        }
    }
}

fn mark_gemini_web_search_history_unavailable(input: &Value, tool_context: &mut CodexToolContext) {
    if !tool_context.accepts_web_search() || tool_context.is_compaction_request() {
        return;
    }
    let Some(items) = input.get("input").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("web_search_call") {
            continue;
        }
        if let Some(response_item_id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            tool_context.mark_web_search_shadow_unavailable(response_item_id.to_string());
        }
    }
}

fn apply_gemini_thinking_level(
    gemini_body: &mut Value,
    model: &str,
    effort: Option<&str>,
    compat: &GeminiCompat,
) -> Result<(), ProxyError> {
    if compat.thinking != GeminiThinking::Auto
        || !crate::proxy::providers::transform_gemini::supports_gemini_thinking_level(model)
    {
        return Ok(());
    }
    let Some(effort) = effort.map(str::trim).filter(|effort| !effort.is_empty()) else {
        return Ok(());
    };
    let level = match effort.to_ascii_lowercase().as_str() {
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" | "xhigh" | "max" | "ultra" => "HIGH",
        "none" | "off" | "disabled" => return Ok(()),
        _ => {
            return Err(ProxyError::InvalidRequest(format!(
                "Unsupported reasoning.effort for Gemini: {effort}"
            )))
        }
    };
    gemini_body["generationConfig"]["thinkingConfig"] = json!({ "thinkingLevel": level });
    Ok(())
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
    async fn web_search_shadow_is_not_replayed_after_provider_switch() {
        let history = CodexChatHistoryStore::default();
        history
            .record_anthropic_web_search_shadows(
                "previous-provider",
                vec![
                    crate::proxy::providers::codex_chat_history::AnthropicWebSearchShadow {
                        server_tool_use: json!({
                            "type":"server_tool_use",
                            "id":"srvtoolu_1",
                            "name":"web_search",
                            "input":{"query":"find docs"},
                            "encrypted_content":"previous-signature"
                        }),
                        web_search_tool_result: json!({
                            "type":"web_search_tool_result",
                            "tool_use_id":"srvtoolu_1",
                            "content":[{"type":"web_search_result","url":"https://example.com"}],
                            "encrypted_content":"previous-result"
                        }),
                    },
                ],
            )
            .await;

        let input = json!({
            "model":"claude-test",
            "input":[
                {"type":"message","role":"user","content":"Find it."},
                {"type":"web_search_call","id":"ws_srvtoolu_1","status":"completed","action":{"type":"search","query":"find docs"}},
                {"type":"message","role":"user","content":"Continue."}
            ],
            "tools":[{"type":"web_search"}]
        });
        let prepared = prepare(
            input,
            &provider(Protocol::Messages),
            &HeaderMap::new(),
            &CompatConfig::default(),
            &history,
        )
        .await
        .unwrap();

        assert_eq!(prepared.body["tools"][0]["type"], "web_search_20250305");
        let messages = prepared.body["messages"].to_string();
        assert!(!messages.contains("server_tool_use"));
        assert!(!messages.contains("web_search_tool_result"));
        assert!(!messages.contains("previous-signature"));
        assert!(!messages.contains("previous-result"));
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
            None,
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
            None,
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
            None,
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
            None,
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

    #[tokio::test]
    async fn gemini_omits_bridge_default_but_keeps_explicit_output_limits() {
        let provider = provider(Protocol::GeminiNative);
        let headers = HeaderMap::new();
        let global = CompatConfig::default();
        let history = CodexChatHistoryStore::default();

        let prepare_body = |max_output_tokens| {
            let mut input = json!({
                "model": "gemini-2.5-flash",
                "input": "hello"
            });
            if let Some(value) = max_output_tokens {
                input["max_output_tokens"] = json!(value);
            }
            input
        };

        let mut implicit_context = build_context(
            provider.protocol,
            &provider,
            &headers,
            &prepare_body(None),
            None,
        );
        let implicit = prepare_attempt(
            &mut implicit_context,
            prepare_body(None),
            &provider,
            &headers,
            &global,
            &history,
            None,
        )
        .await
        .unwrap();
        assert!(implicit.body["generationConfig"]
            .get("maxOutputTokens")
            .is_none());

        let mut explicit_context = build_context(
            provider.protocol,
            &provider,
            &headers,
            &prepare_body(Some(8192)),
            None,
        );
        let explicit = prepare_attempt(
            &mut explicit_context,
            prepare_body(Some(8192)),
            &provider,
            &headers,
            &global,
            &history,
            None,
        )
        .await
        .unwrap();
        assert_eq!(explicit.body["generationConfig"]["maxOutputTokens"], 8192);
    }

    #[tokio::test]
    async fn gemini_media_retry_reuses_endpoint_session_headers_and_generation_options() {
        let mut provider = provider(Protocol::GeminiNative);
        provider.base_url = "https://generativelanguage.googleapis.com/v1beta".into();
        let mut headers = HeaderMap::new();
        headers.insert(
            "session-id",
            HeaderValue::from_static("client-session-12345678901234567890"),
        );
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer client-credential"),
        );
        headers.insert("x-api-key", HeaderValue::from_static("client-api-key"));
        headers.insert(
            "x-goog-api-key",
            HeaderValue::from_static("client-google-key"),
        );
        let input = json!({
            "model": "gemini-2.5-flash",
            "stream": true,
            "max_output_tokens": 512,
            "temperature": 0.2,
            "top_p": 0.7,
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "describe"},
                    {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="}
                ]
            }]
        });
        let global = CompatConfig::default();
        let history = CodexChatHistoryStore::default();
        let shadow = GeminiShadowStore::default();
        let mut context = build_context(
            provider.protocol,
            &provider,
            &headers,
            &input,
            Some("alt=client-value&trace=client"),
        );
        let session = context.gemini_session_id.clone();
        let first = prepare_attempt(
            &mut context,
            input.clone(),
            &provider,
            &headers,
            &global,
            &history,
            Some(&shadow),
        )
        .await
        .unwrap();

        let error = ProxyError::UpstreamError {
            status: 400,
            body: Some(
                json!({"error":{"message":"This model does not support image input"}}).to_string(),
            ),
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
            Some(&shadow),
        )
        .await
        .unwrap();

        assert_eq!(
            first.url,
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
        );
        assert_eq!(first.url, second.url);
        assert!(first.request_is_streaming);
        assert_eq!(first.request_is_streaming, second.request_is_streaming);
        assert_eq!(first.headers, second.headers);
        assert_eq!(first.headers["x-goog-api-key"], "test-key");
        assert!(!first.headers.contains_key("authorization"));
        assert!(!first.headers.contains_key("x-api-key"));
        assert_eq!(context.gemini_session_id, session);
        assert_ne!(first.body, second.body);
        assert_eq!(
            first.body["generationConfig"],
            second.body["generationConfig"]
        );
        assert!(first.body.to_string().contains("inlineData"));
        assert!(second.body.to_string().contains("[Unsupported Image]"));
        assert!(!second.body.to_string().contains("inlineData"));
    }

    #[test]
    fn gemini_thinking_effort_only_maps_to_supported_model_levels() {
        let compat = GeminiCompat::default();

        let mut gemini_3 = json!({});
        apply_gemini_thinking_level(&mut gemini_3, "models/gemini-3-pro", Some("high"), &compat)
            .unwrap();
        assert_eq!(
            gemini_3["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );

        let mut gemini_2 = json!({});
        apply_gemini_thinking_level(&mut gemini_2, "gemini-2.5-flash", Some("high"), &compat)
            .unwrap();
        assert!(gemini_2.get("generationConfig").is_none());

        let mut disabled = json!({});
        apply_gemini_thinking_level(
            &mut disabled,
            "gemini-3-pro",
            Some("high"),
            &GeminiCompat {
                thought_signature_replay: true,
                thinking: GeminiThinking::Off,
            },
        )
        .unwrap();
        assert!(disabled.get("generationConfig").is_none());
    }
}
