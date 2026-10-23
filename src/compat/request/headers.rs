//! Outbound headers and authentication for the selected provider.
use super::forwarder::*;
use crate::{
    config::{Protocol, Provider},
    proxy::error::ProxyError,
};
use axum::http::{self, HeaderMap, HeaderValue};
use serde_json::Value;
// The adapter supplies the original forwarder locals without changing header order.
#[allow(clippy::too_many_arguments)]
pub(super) fn prepare_headers(
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
