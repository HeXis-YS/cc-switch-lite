// Derived from cc-switch@214e75ba7f625c679dd4e61df166ec4463245fe7.
// Copyright (c) 2025 Jason Young. MIT; see proxy/providers/LICENSE-cc-switch.
use crate::provider::LocalProxyRequestOverrides;
use crate::proxy::{
    body_filter::filter_private_params_with_whitelist, json_canonical::canonicalize_value,
    types::OptimizerConfig,
};
use axum::http;
use serde_json::Value;
const CLAUDE_CODE_SYSTEM_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";
pub(super) fn prepend_claude_code_system_prompt(body: &mut Value) {
    let identity = serde_json::json!({ "type": "text", "text": CLAUDE_CODE_SYSTEM_IDENTITY });
    let mut blocks: Vec<Value> = vec![identity];
    match body.get("system") {
        Some(Value::String(existing)) if !existing.is_empty() => {
            blocks.push(serde_json::json!({ "type": "text", "text": existing }));
        }
        Some(Value::Array(existing)) => {
            // Idempotent: skip re-injection if the first block is already the identity line.
            if existing
                .first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                == Some(CLAUDE_CODE_SYSTEM_IDENTITY)
            {
                return;
            }
            blocks.extend(existing.iter().cloned());
        }
        _ => {}
    }
    body["system"] = Value::Array(blocks);
}

pub(super) fn base_url_is_full_endpoint(base_url: &str, endpoint_suffix: &str) -> bool {
    let trimmed = base_url.trim();
    // Match against the path only: a `?query`/`#fragment` on a full endpoint URL must not
    // hide the suffix (`.../v1/messages?beta=true` still ends in the endpoint).
    let path = match trimmed.split_once(['?', '#']) {
        Some((head, _)) => head,
        None => trimmed,
    };
    path.trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with(endpoint_suffix)
}

pub(super) fn is_codex_client_fingerprint_header(key_str: &str) -> bool {
    matches!(
        key_str,
        "originator"
            | "session_id"
            | "session-id"
            | "thread-id"
            | "conversation_id"
            | "chatgpt-account-id"
            | "x-client-request-id"
            | "openai-beta"
            | "openai-organization"
            | "openai-project"
            // Codex 内置 openai 的固定请求头（值是 Codex 版本号）。
            | "version"
    ) || key_str.starts_with("x-stainless-")
        || key_str.starts_with("x-codex-")
        // ChatGPT 登录态下 Codex 每个请求都带：`x-oai-attestation`（设备证明）、
        // `x-openai-fedramp`、`x-openai-account-routing-override`、`x-openai-memgen-request`、
        // `x-openai-internal-codex-responses-lite`、`x-responsesapi-include-timing-metrics`。
        || key_str.starts_with("x-oai-")
        || key_str.starts_with("x-openai-")
        || key_str.starts_with("x-responsesapi-")
}

pub(super) fn should_strip_codex_client_fingerprint_header(
    key_str: &str,
    endpoint: &str,
    codex_responses_to_anthropic: bool,
    codex_responses_to_chat: bool,
    codex_stack_request: bool,
    codex_official_auth_passthrough: bool,
) -> bool {
    if codex_responses_to_anthropic {
        return is_codex_client_fingerprint_header(key_str);
    }

    if codex_stack_request && !codex_official_auth_passthrough {
        let native_third_party_responses =
            !codex_responses_to_chat && super::codex::is_codex_responses_endpoint(endpoint);
        return is_codex_client_fingerprint_header(key_str)
            && !(native_third_party_responses && key_str == "x-codex-beta-features");
    }

    false
}

pub(super) fn is_streaming_request(
    endpoint: &str,
    body: &Value,
    headers: &axum::http::HeaderMap,
) -> bool {
    if body
        .get("stream")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return true;
    }

    if endpoint.contains("streamGenerateContent") || endpoint.contains("alt=sse") {
        return true;
    }

    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .map(|accept| accept.contains("text/event-stream"))
        .unwrap_or(false)
}

pub(super) fn apply_local_proxy_body_overrides(
    body: &mut Value,
    overrides: &LocalProxyRequestOverrides,
) -> bool {
    let Some(override_body) = overrides.body.as_ref() else {
        return false;
    };

    if !override_body.is_object() {
        log::warn!("[LocalProxyOverrides] Ignoring body override because it is not an object");
        return false;
    }

    merge_json_override(body, override_body)
}

pub(super) fn merge_json_override(target: &mut Value, patch: &Value) -> bool {
    merge_json_override_inner(target, patch, true)
}

pub(super) fn merge_json_override_inner(
    target: &mut Value,
    patch: &Value,
    is_top_level: bool,
) -> bool {
    match (target, patch) {
        (Value::Object(target_map), Value::Object(patch_map)) => {
            let mut changed = false;
            for (key, patch_value) in patch_map {
                if is_top_level && key == "stream" {
                    log::warn!(
                        "[LocalProxyOverrides] Ignoring body override for protected field: stream"
                    );
                    continue;
                }
                match target_map.get_mut(key) {
                    Some(target_value) => {
                        changed |= merge_json_override_inner(target_value, patch_value, false);
                    }
                    None => {
                        target_map.insert(key.clone(), patch_value.clone());
                        changed = true;
                    }
                }
            }
            changed
        }
        (target_value, patch_value) => {
            if target_value == patch_value {
                false
            } else {
                *target_value = patch_value.clone();
                true
            }
        }
    }
}

pub(super) fn apply_local_proxy_header_overrides(
    headers: &mut http::HeaderMap,
    overrides: Option<&LocalProxyRequestOverrides>,
    is_copilot: bool,
) {
    if is_copilot {
        return;
    }

    let Some(header_overrides) = overrides.map(|overrides| &overrides.headers) else {
        return;
    };

    for (raw_name, raw_value) in header_overrides {
        let header_name = raw_name.trim().to_ascii_lowercase();
        if header_name.is_empty() {
            log::warn!("[LocalProxyOverrides] Ignoring header override with empty name");
            continue;
        }

        let Ok(name) = http::HeaderName::from_bytes(header_name.as_bytes()) else {
            log::warn!("[LocalProxyOverrides] Ignoring invalid header override name: {raw_name}");
            continue;
        };

        if is_protected_local_proxy_override_header(&name) {
            log::debug!(
                "[LocalProxyOverrides] Ignoring protected header override: {}",
                name.as_str()
            );
            continue;
        }

        let Ok(value) = http::HeaderValue::from_str(raw_value) else {
            log::warn!(
                "[LocalProxyOverrides] Ignoring invalid header override value for {}",
                name.as_str()
            );
            continue;
        };

        headers.insert(name, value);
    }
}

pub(super) fn is_protected_local_proxy_override_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "te"
            | "trailer"
            | "upgrade"
            | "accept-encoding"
            | "content-type"
            | "authorization"
            | "x-api-key"
            | "x-goog-api-key"
            | "chatgpt-account-id"
            | "session_id"
            | "x-client-request-id"
            | "x-codex-window-id"
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
    )
}

pub(super) fn prepare_upstream_request_body(request_body: Value) -> Value {
    canonicalize_value(filter_private_params_with_whitelist(request_body, &[]))
}

pub(super) fn codex_anthropic_cache_config(config: &OptimizerConfig) -> OptimizerConfig {
    OptimizerConfig {
        enabled: true,
        thinking_optimizer: false,
        cache_injection: config.cache_injection,
    }
}
pub(super) fn build_url(base_url: &str, endpoint: &str) -> String {
    let base_trimmed = base_url.trim_end_matches('/');
    let endpoint_trimmed = endpoint.trim_start_matches('/');

    // OpenAI/Codex 的 base_url 可能是：
    // - 纯 origin: https://api.openai.com  (需要自动补 /v1)
    // - 已含 /v1: https://api.openai.com/v1 (直接拼接)
    // - 自定义前缀: https://xxx/openai (不添加 /v1，直接拼接)

    // 检查 base_url 是否已经包含 /v1
    let already_has_v1 = base_trimmed.ends_with("/v1");
    let origin_only = super::codex::is_origin_only_url(base_trimmed);

    let mut url = if already_has_v1 {
        // 已经有 /v1，直接拼接
        format!("{base_trimmed}/{endpoint_trimmed}")
    } else if origin_only {
        // 纯 origin，添加 /v1
        format!("{base_trimmed}/v1/{endpoint_trimmed}")
    } else {
        // 自定义前缀，不添加 /v1，直接拼接
        format!("{base_trimmed}/{endpoint_trimmed}")
    };

    // 去除重复的 /v1/v1（可能由 base_url 与 endpoint 都带版本导致）
    while url.contains("/v1/v1") {
        url = url.replace("/v1/v1", "/v1");
    }

    url
}
pub(super) fn strip_one_m_suffix_for_upstream(model: &str) -> &str {
    let trimmed = model.trim_end();
    let marker = "[1m]".as_bytes();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= marker.len()
        && bytes[bytes.len() - marker.len()..].eq_ignore_ascii_case(marker)
    {
        return trimmed[..trimmed.len() - marker.len()].trim_end();
    }
    model
}

use sha2::{Digest, Sha256};

pub(super) fn is_opencode_upstream(upstream_host: Option<&str>) -> bool {
    upstream_host
        .map(|host| {
            let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
            host.trim_matches(['[', ']']).to_ascii_lowercase()
        })
        .is_some_and(|host| host == "opencode.ai" || host.ends_with(".opencode.ai"))
}

pub(super) fn conversation_fingerprint(body: &Value) -> Option<String> {
    let mut hasher = Sha256::new();
    let mut fed = false;
    if let Some(system) = body.get("system") {
        hasher.update(without_cache_control(system).to_string().as_bytes());
        fed = true;
    }
    if let Some(first_user) = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|messages| {
            messages
                .iter()
                .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        })
    {
        hasher.update(without_cache_control(first_user).to_string().as_bytes());
        fed = true;
    }
    if !fed {
        return None;
    }
    let digest = format!("{:x}", hasher.finalize());
    Some(format!("ccsw-{}", &digest[..32]))
}

pub(super) fn native_session_header(headers: &http::HeaderMap) -> Option<&str> {
    [
        "x-claude-code-session-id",
        "session-id",
        "session_id",
        "x-session-id",
    ]
    .iter()
    .find_map(|name| {
        headers
            .get(*name)?
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

pub(super) fn pick_opencode_session<'a>(
    native: Option<&'a str>,
    client_provided: Option<&'a str>,
    fingerprint: Option<&'a str>,
    generated: &'a str,
) -> Option<String> {
    [native, client_provided, fingerprint, Some(generated)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn without_cache_control(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "cache_control")
                .map(|(key, value)| (key.clone(), without_cache_control(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_cache_control).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn prepare_upstream_request_body_filters_private_fields_and_canonicalizes_order() {
        let body = json!({
            "z": 1,
            "_internal": "drop",
            "tools": [
                {
                    "name": "lookup",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "_id": {
                                "_private_note": "drop",
                                "type": "string"
                            },
                            "b": {"type": "number"},
                            "a": {"type": "string"}
                        }
                    }
                }
            ],
            "a": 2
        });

        let prepared = prepare_upstream_request_body(body);

        assert!(prepared.get("_internal").is_none());
        assert!(prepared["tools"][0]["parameters"]["properties"]
            .get("_id")
            .is_some());
        assert!(prepared["tools"][0]["parameters"]["properties"]["_id"]
            .get("_private_note")
            .is_none());
        assert_eq!(
            serde_json::to_string(&prepared).unwrap(),
            r#"{"a":2,"tools":[{"name":"lookup","parameters":{"properties":{"_id":{"type":"string"},"a":{"type":"string"},"b":{"type":"number"}},"type":"object"}}],"z":1}"#
        );
    }

    #[test]
    fn local_proxy_body_overrides_deep_merge_final_body_without_stream() {
        let mut body = json!({
            "model": "before",
            "stream": false,
            "metadata": {
                "keep": true,
                "temperature": 1
            },
            "messages": [{ "role": "user", "content": "hello" }]
        });
        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::new(),
            body: Some(json!({
                "model": "after",
                "stream": true,
                "metadata": {
                    "temperature": 0.2,
                    "top_p": 0.9
                },
                "messages": []
            })),
        };

        assert!(apply_local_proxy_body_overrides(&mut body, &overrides));

        assert_eq!(body["model"], "after");
        assert_eq!(body["stream"], false);
        assert_eq!(body["metadata"]["keep"], true);
        assert_eq!(body["metadata"]["temperature"], 0.2);
        assert_eq!(body["metadata"]["top_p"], 0.9);
        assert_eq!(body["messages"], json!([]));
    }

    #[test]
    fn local_proxy_header_overrides_replace_allowed_headers_only() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("original"),
        );
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer good"),
        );
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::from([
                ("User-Agent".to_string(), "custom".to_string()),
                ("X-Test".to_string(), "ok".to_string()),
                ("Authorization".to_string(), "Bearer bad".to_string()),
                ("Content-Type".to_string(), "text/plain".to_string()),
                ("X-Bad".to_string(), "bad\nvalue".to_string()),
            ]),
            body: None,
        };

        apply_local_proxy_header_overrides(&mut headers, Some(&overrides), false);

        assert_eq!(
            headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("custom")
        );
        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer good")
        );
        assert_eq!(
            headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            headers.get("x-test").and_then(|value| value.to_str().ok()),
            Some("ok")
        );
        assert!(headers.get("x-bad").is_none());
    }

    #[test]
    fn local_proxy_header_overrides_are_skipped_for_copilot() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("copilot"),
        );
        let overrides = LocalProxyRequestOverrides {
            headers: HashMap::from([("User-Agent".to_string(), "custom".to_string())]),
            body: None,
        };

        apply_local_proxy_header_overrides(&mut headers, Some(&overrides), true);

        assert_eq!(
            headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some("copilot")
        );
    }

    #[test]
    fn prepend_claude_code_system_prompt_from_string() {
        let mut body = json!({ "system": "You are a Codex agent." });
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "You are a Codex agent.");
    }

    #[test]
    fn prepend_claude_code_system_prompt_when_absent() {
        let mut body = json!({});
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
    }

    #[test]
    fn prepend_claude_code_system_prompt_is_idempotent() {
        let mut body = json!({ "system": "orig" });
        prepend_claude_code_system_prompt(&mut body);
        prepend_claude_code_system_prompt(&mut body);
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], CLAUDE_CODE_SYSTEM_IDENTITY);
        assert_eq!(system[1]["text"], "orig");
    }

    #[test]
    fn codex_client_fingerprint_headers_are_dropped_for_anthropic_upstreams() {
        // Codex/OpenAI fingerprints a native Claude Code client never sends → must drop.
        for header in [
            "originator",
            "session_id",
            "session-id",
            "thread-id",
            "conversation_id",
            "chatgpt-account-id",
            "x-openai-subagent",
            "x-client-request-id",
            "x-codex-window-id",
            "openai-beta",
            "openai-organization",
            "openai-project",
            "version",
            "x-stainless-lang",
            "x-stainless-runtime",
            "x-codex-turn-id",
            // ChatGPT 登录态下的身份头，Stack 请求发往第三方时同样不能带出去。
            "x-oai-attestation",
            "x-openai-fedramp",
            "x-openai-account-routing-override",
            "x-openai-memgen-request",
            "x-openai-internal-codex-responses-lite",
            "x-responsesapi-include-timing-metrics",
        ] {
            assert!(
                is_codex_client_fingerprint_header(header),
                "expected {header} to be dropped while impersonating Claude Code"
            );
        }

        // Headers a real Claude Code client sends (or that the forwarder rebuilds) must
        // NOT be caught by the denylist.
        for header in [
            "anthropic-version",
            "anthropic-beta",
            "user-agent",
            "accept",
            "content-type",
            "x-app",
        ] {
            assert!(
                !is_codex_client_fingerprint_header(header),
                "{header} must be preserved while impersonating Claude Code"
            );
        }
    }

    #[test]
    fn conversation_fingerprint_is_stable_per_conversation() {
        let turn1 = json!({
            "system": "You are an assistant.",
            "messages": [{"role": "user", "content": "first question"}]
        });
        // Later turns of the same conversation append messages; system and the
        // first user message stay put.
        let turn2 = json!({
            "system": "You are an assistant.",
            "messages": [
                {"role": "user", "content": "first question"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "follow-up"}
            ]
        });
        let other = json!({
            "system": "You are an assistant.",
            "messages": [{"role": "user", "content": "another conversation"}]
        });

        let a = conversation_fingerprint(&turn1).expect("fingerprint");
        let b = conversation_fingerprint(&turn2).expect("fingerprint");
        let c = conversation_fingerprint(&other).expect("fingerprint");
        assert_eq!(a, b, "one conversation must keep a single session id");
        assert_ne!(a, c, "different conversations must not share a session id");
        assert!(
            a.starts_with("ccsw-"),
            "prefix keeps the origin recognisable: {a}"
        );
    }

    #[test]
    fn conversation_fingerprint_requires_conversation_content() {
        assert!(conversation_fingerprint(&json!({"model": "x"})).is_none());
    }

    #[test]
    fn conversation_fingerprint_ignores_cache_control() {
        // Clients move `cache_control` between messages as a conversation grows
        // (Claude Code puts it on the first user message, then on the last one);
        // the session id must not change with it.
        let first = json!({
            "system": "You are an assistant.",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}
                ]
            }]
        });
        let later = json!({
            "system": "You are an assistant.",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": [
                    {"type": "text", "text": "again", "cache_control": {"type": "ephemeral"}}
                ]}
            ]
        });
        assert_eq!(
            conversation_fingerprint(&first),
            conversation_fingerprint(&later),
            "cache_control placement must not change the session id"
        );
    }

    #[test]
    fn opencode_session_precedence() {
        // native client header > client-provided id > fingerprint > generated id
        assert_eq!(
            pick_opencode_session(Some("native"), Some("client"), Some("fp"), "gen").as_deref(),
            Some("native")
        );
        assert_eq!(
            pick_opencode_session(None, Some("client"), Some("fp"), "gen").as_deref(),
            Some("client")
        );
        assert_eq!(
            pick_opencode_session(None, None, Some("fp"), "gen").as_deref(),
            Some("fp")
        );
        assert_eq!(
            pick_opencode_session(None, None, None, "gen").as_deref(),
            Some("gen")
        );
        // Blank candidates are skipped, and nothing is sent when all are blank.
        assert_eq!(
            pick_opencode_session(Some("   "), None, None, "gen").as_deref(),
            Some("gen")
        );
        assert_eq!(pick_opencode_session(None, None, None, "  "), None);
    }

    #[test]
    fn native_session_header_recognises_real_client_headers() {
        // Claude Code and Codex each send one of these; unknown headers do not count.
        for name in [
            "x-claude-code-session-id",
            "session-id",
            "session_id",
            "x-session-id",
        ] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::HeaderName::from_static(name),
                http::HeaderValue::from_static("abc123"),
            );
            assert_eq!(native_session_header(&headers), Some("abc123"), "{name}");
        }

        let mut blank = http::HeaderMap::new();
        blank.insert(
            http::HeaderName::from_static("session_id"),
            http::HeaderValue::from_static("  "),
        );
        assert_eq!(native_session_header(&blank), None);

        let mut other = http::HeaderMap::new();
        other.insert(
            http::HeaderName::from_static("x-request-id"),
            http::HeaderValue::from_static("ignored"),
        );
        assert_eq!(native_session_header(&other), None);
        assert_eq!(native_session_header(&http::HeaderMap::new()), None);
    }
}

use crate::proxy::error::ProxyError;
const PROXY_AUTH_PLACEHOLDER: &str = "PROXY_MANAGED";

pub(super) fn reject_proxy_placeholder_for_managed_account_upstream(
    url: &str,
    headers: &http::HeaderMap,
) -> Result<(), ProxyError> {
    if !is_managed_account_upstream_url(url) || !headers_contain_proxy_placeholder(headers) {
        return Ok(());
    }

    Err(ProxyError::AuthError(
        "Managed account proxy auth was not resolved; PROXY_MANAGED must not be sent upstream"
            .to_string(),
    ))
}

pub(super) fn is_managed_account_upstream_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<http::Uri>() else {
        return false;
    };

    let Some(host) = uri.host().map(str::to_ascii_lowercase) else {
        return false;
    };

    host == "githubcopilot.com"
        || host.ends_with(".githubcopilot.com")
        || (host == "chatgpt.com" && uri.path().starts_with("/backend-api/codex"))
        || (host == "api.x.ai" && uri.path().starts_with("/v1/"))
}

pub(super) fn headers_contain_proxy_placeholder(headers: &http::HeaderMap) -> bool {
    headers.values().any(|value| {
        value
            .to_str()
            .map(|value| value.contains(PROXY_AUTH_PLACEHOLDER))
            .unwrap_or(false)
    })
}

pub(super) fn append_query_to_full_url(base_url: &str, query: Option<&str>) -> String {
    match query {
        Some(query) if !query.is_empty() => {
            if base_url.contains('?') {
                format!("{base_url}&{query}")
            } else {
                format!("{base_url}?{query}")
            }
        }
        _ => base_url.to_string(),
    }
}
pub fn normalize_thinking_type(body: Value) -> Value {
    body
}
#[cfg(test)]
mod normalization_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn test_normalize_thinking_type_adaptive_unchanged() {
        let body = json!({
            "model": "claude-test",
            "thinking": { "type": "adaptive" }
        });

        let result = normalize_thinking_type(body);

        assert_eq!(result["thinking"]["type"], "adaptive");
        assert!(result["thinking"].get("budget_tokens").is_none());
    }
    #[test]
    fn test_normalize_thinking_type_enabled_unchanged() {
        let body = json!({
            "model": "claude-test",
            "thinking": { "type": "enabled", "budget_tokens": 2048 }
        });

        let result = normalize_thinking_type(body);

        assert_eq!(result["thinking"]["type"], "enabled");
        assert_eq!(result["thinking"]["budget_tokens"], 2048);
    }
    #[test]
    fn test_normalize_thinking_type_disabled_unchanged() {
        let body = json!({
            "model": "claude-test",
            "thinking": { "type": "disabled" }
        });

        let result = normalize_thinking_type(body);

        assert_eq!(result["thinking"]["type"], "disabled");
    }
    #[test]
    fn test_normalize_thinking_type_preserves_budget() {
        let body = json!({
            "model": "claude-test",
            "thinking": { "type": "adaptive", "budget_tokens": 5000 }
        });

        let result = normalize_thinking_type(body);

        assert_eq!(result["thinking"]["type"], "adaptive");
        assert_eq!(result["thinking"]["budget_tokens"], 5000);
    }
    #[test]
    fn test_normalize_thinking_type_no_thinking() {
        let body = json!({
            "model": "claude-test"
        });

        let result = normalize_thinking_type(body);

        assert!(result.get("thinking").is_none());
    }
    #[test]
    fn test_normalize_thinking_type_unknown_unchanged() {
        let body = json!({
            "model": "claude-test",
            "thinking": { "type": "unexpected", "budget_tokens": 100 }
        });

        let result = normalize_thinking_type(body);

        assert_eq!(result["thinking"]["type"], "unexpected");
        assert_eq!(result["thinking"]["budget_tokens"], 100);
    }
}
