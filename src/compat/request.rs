// Copyright (c) 2025 Jason Young. MIT; see ../proxy/providers/LICENSE-cc-switch.
//! Single API-key provider projection of cc-switch's Codex forwarder.
//! Source: 214e75ba7f625c679dd4e61df166ec4463245fe7; see upstream_request.
use super::CompatConfig;
use crate::config::{Protocol, Provider};
use crate::provider::{CodexChatReasoningConfig, LocalProxyRequestOverrides};
use crate::proxy::{
    error::ProxyError,
    providers::{self, codex_chat_history::CodexChatHistoryStore},
};
use axum::http::{self, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[path = "upstream_request/codex.rs"]
mod codex;
#[path = "upstream_request/forwarder.rs"]
mod forwarder;
use forwarder::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CompatMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_chat_reasoning: Option<CodexChatReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_routing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impersonate_claude_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_full_url: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_proxy_request_overrides: Option<LocalProxyRequestOverrides>,
}
fn empty_object() -> Value {
    json!({})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCompat {
    pub meta: CompatMeta,
    #[serde(default = "empty_object")]
    pub settings_config: Value,
}
impl Default for ProviderCompat {
    fn default() -> Self {
        Self {
            meta: CompatMeta::default(),
            settings_config: empty_object(),
        }
    }
}

/// Supply the same upstream facts to copied code without allowing a second URL,
/// authentication source or model routing configuration.
pub fn upstream_provider(provider: &Provider) -> crate::provider::Provider {
    let mut settings = provider.compat.settings_config.clone();
    settings["base_url"] = json!(provider.base_url);
    let wire = match provider.protocol {
        Protocol::Responses => "responses",
        Protocol::ChatCompletions => "chat",
        Protocol::Messages => "anthropic",
    };
    settings["config"] = json!(format!(
        "base_url = {}\nwire_api = {}\n",
        toml::Value::String(provider.base_url.clone()),
        toml::Value::String(wire.into())
    ));
    crate::provider::Provider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        settings_config: settings,
        meta: Some(provider.compat.meta.clone()),
        ..Default::default()
    }
}

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
            let mut body =
                providers::transform_codex_anthropic::responses_request_to_anthropic(input, 8192)?;
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

// The adapter supplies the original forwarder locals without changing header order.
#[allow(clippy::too_many_arguments)]
fn prepare_headers(
    provider: &Provider,
    headers: &HeaderMap,
    body: &Value,
    url: &str,
    effective_endpoint: &str,
    codex_impersonate_claude_code: bool,
    codex_anthropic_one_m: bool,
    session: Option<&str>,
    fingerprint: Option<&str>,
    generated_session: &str,
) -> Result<HeaderMap, ProxyError> {
    let codex_responses_to_anthropic = provider.protocol == Protocol::Messages;
    let codex_responses_to_chat = provider.protocol == Protocol::ChatCompletions;
    // Scope bindings: API-key routing has no official-login, stack or Copilot branch.
    let codex_official_auth_passthrough = false;
    let codex_stack_request = false;
    let is_copilot = false;
    let copilot_fingerprint_headers: &[&str] = &[];
    let should_send_anthropic_headers = false;
    let force_identity_encoding = codex_responses_to_chat
        || codex_responses_to_anthropic
        || is_streaming_request(effective_endpoint, body, headers);
    let upstream_host = url
        .parse::<http::Uri>()
        .ok()
        .and_then(|u| u.authority().map(|a| a.to_string()));
    let auth_name = if codex_responses_to_anthropic {
        "x-api-key"
    } else {
        "authorization"
    };
    let auth_value = if codex_responses_to_anthropic {
        provider.api_key.clone()
    } else {
        format!("Bearer {}", provider.api_key)
    };
    let mut auth_headers = vec![(
        http::HeaderName::from_static(auth_name),
        HeaderValue::from_str(&auth_value)
            .map_err(|_| ProxyError::TransformError("Invalid upstream credential".into()))?,
    )];
    let custom_user_agent = provider
        .compat
        .meta
        .custom_user_agent
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| HeaderValue::from_str(v).ok())
        .or_else(|| {
            codex_impersonate_claude_code
                .then(|| HeaderValue::from_static("claude-cli/1.0.119 (external, cli)"))
        });
    let opencode_fallback_user_agent =
        if custom_user_agent.is_none() && is_opencode_upstream(upstream_host.as_deref()) {
            Some(HeaderValue::from_static(concat!(
                "cc-switch/",
                env!("CARGO_PKG_VERSION")
            )))
        } else {
            None
        };
    if is_opencode_upstream(upstream_host.as_deref()) && !headers.contains_key("x-opencode-session")
    {
        if let Some(session) = pick_opencode_session(
            native_session_header(headers),
            session,
            fingerprint,
            generated_session,
        ) {
            if let Ok(value) = HeaderValue::from_str(&session) {
                auth_headers.push((http::HeaderName::from_static("x-opencode-session"), value));
            }
        }
    }
    let anthropic_beta_value = if codex_impersonate_claude_code || codex_anthropic_one_m {
        let mut betas = Vec::new();
        if codex_impersonate_claude_code {
            betas.push("claude-code-20250219");
        }
        if codex_anthropic_one_m {
            betas.push("context-1m-2025-08-07");
        }
        Some(betas.join(","))
    } else {
        None
    };
    let mut ordered_headers = http::HeaderMap::new();
    let mut saw_auth = false;
    let mut saw_accept_encoding = false;
    let mut saw_accept = false;
    let mut saw_user_agent = false;
    let mut saw_anthropic_beta = false;
    let mut saw_anthropic_version = false;

    for (key, value) in headers {
        let key_str = key.as_str();

        // --- host — 原位替换为上游 host（保持客户端原始位置） ---
        if key_str.eq_ignore_ascii_case("host") {
            if let Some(ref host_val) = upstream_host {
                if let Ok(hv) = http::HeaderValue::from_str(host_val) {
                    ordered_headers.append(key.clone(), hv);
                }
            }
            continue;
        }

        // --- 连接 / 追踪 / CDN 类 — 无条件跳过 ---
        if matches!(
            key_str,
            "content-length"
                | "transfer-encoding"
                | "x-forwarded-host"
                | "x-forwarded-port"
                | "x-forwarded-proto"
                | "forwarded"
                | "cf-connecting-ip"
                | "cf-ipcountry"
                | "cf-ray"
                | "cf-visitor"
                | "true-client-ip"
                | "fastly-client-ip"
                | "x-azure-clientip"
                | "x-azure-fdid"
                | "x-azure-ref"
                | "akamai-origin-hop"
                | "x-akamai-config-log-detail"
                | "x-request-id"
                | "x-correlation-id"
                | "x-trace-id"
                | "x-amzn-trace-id"
                | "x-b3-traceid"
                | "x-b3-spanid"
                | "x-b3-parentspanid"
                | "x-b3-sampled"
                | "traceparent"
                | "tracestate"
        ) {
            continue;
        }

        // --- 认证类 — 用 adapter 提供的认证头替换（在原始位置） ---
        if key_str.eq_ignore_ascii_case("authorization")
            || key_str.eq_ignore_ascii_case("x-api-key")
            || key_str.eq_ignore_ascii_case("x-goog-api-key")
        {
            // Codex official account cards deliberately keep credentials
            // out of provider storage. `requires_openai_auth = true` makes
            // Codex send the active ChatGPT authorization, which must reach
            // the official upstream unchanged. Other credential headers
            // are still discarded.
            if codex_official_auth_passthrough && key_str.eq_ignore_ascii_case("authorization") {
                saw_auth = true;
                ordered_headers.append(key.clone(), value.clone());
                continue;
            }
            if !saw_auth {
                saw_auth = true;
                for (ah_name, ah_value) in &auth_headers {
                    ordered_headers.append(ah_name.clone(), ah_value.clone());
                }
            }
            continue;
        }

        // --- x-app — during Codex→Anthropic emulation, `cli` is injected uniformly below ---
        if codex_impersonate_claude_code && key_str.eq_ignore_ascii_case("x-app") {
            continue;
        }

        // --- Codex/OpenAI fingerprint headers — never leak to an Anthropic upstream ---
        // These are client/session identifiers from the incoming Codex request,
        // not Anthropic protocol headers. Forwarding them both leaks identity and
        // can defeat strict gateway fingerprint checks.
        // The full set lives in `is_codex_client_fingerprint_header` so it stays in one
        // place. (HeaderName is lowercased by the http crate, so a direct match is safe.)
        // Codex Stack 请求发往第三方时同样剥掉：官方做路由时 Codex 每个请求都带着 ChatGPT
        // 身份（`chatgpt-account-id` 等），它们只能发往官方上游。Stack 目标不会是官方账号，
        // 这里仍按上游判断，防止以后放宽。只限 Codex：Claude Code 自己的 `x-stainless-*`
        // 是 Anthropic SDK 的正常请求头，Stack 请求照常转发。
        if should_strip_codex_client_fingerprint_header(
            key_str,
            effective_endpoint,
            codex_responses_to_anthropic,
            codex_responses_to_chat,
            codex_stack_request,
            codex_official_auth_passthrough,
        ) {
            continue;
        }

        // --- accept — force application/json on the Codex→Anthropic path ---
        // The Codex CLI sends `Accept: text/event-stream`, whereas a native
        // Anthropic client sends `application/json` (streaming is driven by
        // the body's stream:true). Strict Anthropic gateways return 406 Not
        // Acceptable for an event-stream Accept, so normalize it here.
        if codex_responses_to_anthropic && key_str.eq_ignore_ascii_case("accept") {
            if !saw_accept {
                saw_accept = true;
                ordered_headers.append(
                    http::header::ACCEPT,
                    http::HeaderValue::from_static("application/json"),
                );
            }
            continue;
        }

        // --- accept-encoding — transform / SSE 路径强制 identity，其余保留原值 ---
        if key_str.eq_ignore_ascii_case("accept-encoding") {
            if !saw_accept_encoding {
                saw_accept_encoding = true;
                if force_identity_encoding {
                    ordered_headers.append(
                        http::header::ACCEPT_ENCODING,
                        http::HeaderValue::from_static("identity"),
                    );
                } else {
                    ordered_headers.append(key.clone(), value.clone());
                }
            }
            continue;
        }

        // --- user-agent: provider-level override for local proxy routing ---
        if !is_copilot && key_str.eq_ignore_ascii_case("user-agent") {
            if !saw_user_agent {
                saw_user_agent = true;
                if let Some(ref ua) = custom_user_agent {
                    ordered_headers.append(http::header::USER_AGENT, ua.clone());
                } else {
                    ordered_headers.append(key.clone(), value.clone());
                }
            }
            continue;
        }

        // --- anthropic-beta — 用重建值替换（确保含 claude-code 标记） ---
        if key_str.eq_ignore_ascii_case("anthropic-beta") {
            if !saw_anthropic_beta {
                saw_anthropic_beta = true;
                if let Some(ref beta_val) = anthropic_beta_value {
                    if let Ok(hv) = http::HeaderValue::from_str(beta_val) {
                        ordered_headers.append("anthropic-beta", hv);
                    }
                }
            }
            continue;
        }

        // --- anthropic-version — 透传客户端值 ---
        if key_str.eq_ignore_ascii_case("anthropic-version") {
            if should_send_anthropic_headers {
                saw_anthropic_version = true;
                ordered_headers.append(key.clone(), value.clone());
            }
            continue;
        }

        // --- Copilot 指纹头 — 跳过（由 auth_headers 提供） ---
        if copilot_fingerprint_headers
            .iter()
            .any(|h| key_str.eq_ignore_ascii_case(h))
        {
            continue;
        }

        // --- 默认：透传 ---
        ordered_headers.append(key.clone(), value.clone());
    }

    // 如果原始请求中没有认证头，在末尾追加
    if !saw_auth && !auth_headers.is_empty() {
        for (ah_name, ah_value) in &auth_headers {
            ordered_headers.append(ah_name.clone(), ah_value.clone());
        }
    }

    // transform / SSE 路径在缺失时补 identity；普通透传不主动补 accept-encoding
    if !saw_accept_encoding && force_identity_encoding {
        ordered_headers.append(
            http::header::ACCEPT_ENCODING,
            http::HeaderValue::from_static("identity"),
        );
    }

    // On the Codex→Anthropic path, add application/json when Accept is missing (matching a native Anthropic client).
    if codex_responses_to_anthropic && !saw_accept {
        ordered_headers.append(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
    }

    // Codex→Anthropic emulation: inject Claude Code's x-app: cli
    if codex_impersonate_claude_code {
        ordered_headers.append("x-app", http::HeaderValue::from_static("cli"));
    }

    if !saw_user_agent {
        if let Some(ua) = custom_user_agent
            .as_ref()
            .or(opencode_fallback_user_agent.as_ref())
        {
            ordered_headers.append(http::header::USER_AGENT, ua.clone());
        }
    }

    // 如果原始请求中没有 anthropic-beta 且有值需要添加，追加
    if !saw_anthropic_beta {
        if let Some(ref beta_val) = anthropic_beta_value {
            if let Ok(hv) = http::HeaderValue::from_str(beta_val) {
                ordered_headers.append("anthropic-beta", hv);
            }
        }
    }

    // anthropic-version: add the default only when it is missing.
    // The Codex→Anthropic path also needs this header. Note this is independent
    // of anthropic-beta: the Claude Code-specific beta is only sent when
    // impersonation is on (handled above); on the plain Codex→Anthropic path
    // (impersonation off) anthropic-version is still required but no beta is sent.
    if (should_send_anthropic_headers || codex_responses_to_anthropic) && !saw_anthropic_version {
        ordered_headers.append(
            "anthropic-version",
            http::HeaderValue::from_static("2023-06-01"),
        );
    }

    if !ordered_headers.contains_key(http::header::CONTENT_TYPE) {
        ordered_headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    apply_local_proxy_header_overrides(
        &mut ordered_headers,
        provider.compat.meta.local_proxy_request_overrides.as_ref(),
        false,
    );
    reject_proxy_placeholder_for_managed_account_upstream(url, &ordered_headers)?;
    Ok(ordered_headers)
}
