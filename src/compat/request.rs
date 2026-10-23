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
    pub(crate) xai_restore: Option<
        std::collections::HashMap<
            String,
            providers::transform_codex_responses_namespace::NamespacedName,
        >,
    >,
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
    let mut input = normalize_thinking_type(input);
    let upstream = upstream_provider(provider);
    let native_compaction = provider.protocol == Protocol::Responses
        && providers::codex_compaction::is_compaction_request(&input);
    let mut one_m = false;
    let impersonate = provider.protocol == Protocol::Messages
        && provider.compat.meta.impersonate_claude_code == Some(true);
    let xai_native = provider.protocol == Protocol::Responses
        && codex::provider_is_xai_native_responses(&upstream);
    let xai_restore = xai_native
        .then(|| providers::transform_codex_responses_namespace::namespace_restore_map(&input));
    let extracted_session = crate::proxy::session::extract_session_id(headers, &input, "codex");
    let session = extracted_session
        .client_provided
        .then_some(extracted_session.session_id.as_str());
    let fingerprint = if session.is_none()
        && !headers.contains_key("x-opencode-session")
        && native_session_header(headers).is_none()
    {
        conversation_fingerprint(&input)
    } else {
        None
    };
    if provider.protocol != Protocol::Messages {
        if let Some(model) = input.get("model").and_then(Value::as_str) {
            let stripped = strip_one_m_suffix_for_upstream(model);
            if stripped != model {
                input["model"] = json!(stripped);
            }
        }
    }
    let mut body = match provider.protocol {
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
                session,
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
            if let Some(model) = body.get("model").and_then(Value::as_str) {
                let stripped = strip_one_m_suffix_for_upstream(model);
                if stripped != model {
                    one_m = true;
                    body["model"] = json!(stripped);
                }
            }
            if impersonate {
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
            if xai_native {
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
    let endpoint = match provider.protocol {
        Protocol::Responses => "/responses",
        Protocol::ChatCompletions => "/chat/completions",
        Protocol::Messages => "/v1/messages",
    };
    let full = provider.compat.meta.is_full_url == Some(true)
        || (provider.protocol == Protocol::ChatCompletions
            && base_url_is_full_endpoint(&provider.base_url, "/chat/completions"))
        || (provider.protocol == Protocol::Messages
            && base_url_is_full_endpoint(&provider.base_url, "/v1/messages"));
    let effective_endpoint = match query {
        Some(query) if !query.is_empty() => format!("{endpoint}?{query}"),
        _ => endpoint.to_string(),
    };
    let url = if full {
        append_query_to_full_url(&provider.base_url, query)
    } else {
        build_url(&provider.base_url, &effective_endpoint)
    };
    let outbound_headers = prepare_headers(
        provider,
        headers,
        &body,
        &url,
        &effective_endpoint,
        impersonate,
        one_m,
        session,
        fingerprint.as_deref(),
        &extracted_session.session_id,
    )?;
    let request_is_streaming = is_streaming_request(&effective_endpoint, &body, headers);
    Ok(Prepared {
        body,
        headers: outbound_headers,
        url,
        native_compaction,
        request_is_streaming,
        late_arguments_repair: provider.protocol == Protocol::Responses && !xai_native,
        xai_restore,
    })
}
