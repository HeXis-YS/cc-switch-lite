//! Infrastructure fixture bindings; header algorithm below is extracted upstream.
use http::{HeaderMap, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub struct HeaderFacts<'a> {
    pub protocol: &'a str,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub full_url: bool,
    pub impersonate: bool,
    pub custom_user_agent: Option<&'a str>,
    pub version: &'static str,
}

pub fn request_headers(facts: HeaderFacts<'_>, incoming: &HeaderMap, original_body: &Value, body: &Value) -> (String, HeaderMap) {
    let headers = incoming;
    let endpoint = match facts.protocol { "chat" => "/chat/completions", "messages" => "/v1/messages", "responses" => "/responses", _ => panic!("unsupported fixture") };
    let full = facts.full_url || (facts.protocol == "chat" && base_url_is_full_endpoint(facts.base_url, "/chat/completions")) || (facts.protocol == "messages" && base_url_is_full_endpoint(facts.base_url, "/v1/messages"));
    let url = if full { facts.base_url.to_string() } else { build_url(facts.base_url, endpoint) };
    let effective_endpoint = endpoint;
    let codex_responses_to_anthropic = facts.protocol == "messages";
    let codex_responses_to_chat = facts.protocol == "chat";
    let codex_impersonate_claude_code = facts.impersonate && codex_responses_to_anthropic;
    let codex_anthropic_one_m = codex_responses_to_anthropic && original_body.get("model").and_then(Value::as_str).is_some_and(|v| v.trim_end().to_ascii_lowercase().ends_with("[1m]"));
    let codex_official_auth_passthrough = false;
    let codex_stack_request = false;
    let is_copilot = false;
    let should_send_anthropic_headers = false;
    let copilot_fingerprint_headers: &[&str] = &[];
    let force_identity_encoding = codex_responses_to_anthropic || codex_responses_to_chat || is_streaming_request(endpoint, body, headers);
    let upstream_host = url.parse::<http::Uri>().ok().and_then(|u|u.authority().map(|a| a.to_string()));
    let mut auth_headers = if codex_responses_to_anthropic {vec![(http::HeaderName::from_static("x-api-key"), HeaderValue::from_str(facts.api_key).unwrap())]} else {vec![(http::HeaderName::from_static("authorization"), HeaderValue::from_str(&format!("Bearer {}", facts.api_key)).unwrap())]};
    let custom_user_agent = facts.custom_user_agent.map(str::trim).filter(|s| !s.is_empty()).and_then(|s| HeaderValue::from_str(s).ok()).or_else(|| codex_impersonate_claude_code.then(|| HeaderValue::from_static("claude-cli/1.0.119 (external, cli)")));
    let opencode_fallback_user_agent = if custom_user_agent.is_none() && is_opencode_upstream(upstream_host.as_deref()) { Some(HeaderValue::from_str(&format!("cc-switch/{}", facts.version)).unwrap()) } else { None };
    let session = crate::proxy::session::extract_session_id(headers, original_body, "codex");
    let fingerprint = if !session.client_provided && !headers.contains_key("x-opencode-session") && native_session_header(headers).is_none() && is_opencode_upstream(upstream_host.as_deref()) { conversation_fingerprint(original_body) } else { None };
    if is_opencode_upstream(upstream_host.as_deref()) && !headers.contains_key("x-opencode-session") {
        if let Some(value) = pick_opencode_session(native_session_header(headers), session.client_provided.then_some(session.session_id.as_str()), fingerprint.as_deref(), &session.session_id) {
            auth_headers.push((http::HeaderName::from_static("x-opencode-session"), HeaderValue::from_str(&value).unwrap()));
        }
    }
    let anthropic_beta_value = if codex_impersonate_claude_code || codex_anthropic_one_m {
        let mut betas = Vec::new();
        if codex_impersonate_claude_code { betas.push("claude-code-20250219"); }
        if codex_anthropic_one_m { betas.push("context-1m-2025-08-07"); }
        Some(betas.join(","))
    } else { None };
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
                if codex_official_auth_passthrough && key_str.eq_ignore_ascii_case("authorization")
                {
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
                &effective_endpoint,
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
        if (should_send_anthropic_headers || codex_responses_to_anthropic) && !saw_anthropic_version
        {
            ordered_headers.append(
                "anthropic-version",
                http::HeaderValue::from_static("2023-06-01"),
            );
        }


    if !ordered_headers.contains_key(http::header::CONTENT_TYPE) { ordered_headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("application/json")); }
    (url, ordered_headers)
}
fn base_url_is_full_endpoint(base_url: &str, endpoint_suffix: &str) -> bool {
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

fn is_codex_client_fingerprint_header(key_str: &str) -> bool {
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

fn should_strip_codex_client_fingerprint_header(
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
            !codex_responses_to_chat && crate::proxy::providers::is_codex_responses_endpoint(endpoint);
        return is_codex_client_fingerprint_header(key_str)
            && !(native_third_party_responses && key_str == "x-codex-beta-features");
    }

    false
}

fn is_streaming_request(endpoint: &str, body: &Value, headers: &axum::http::HeaderMap) -> bool {
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

fn is_opencode_upstream(upstream_host: Option<&str>) -> bool {
    upstream_host
        .map(|host| {
            let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
            host.trim_matches(['[', ']']).to_ascii_lowercase()
        })
        .is_some_and(|host| host == "opencode.ai" || host.ends_with(".opencode.ai"))
}

fn native_session_header(headers: &http::HeaderMap) -> Option<&str> {
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

fn pick_opencode_session<'a>(
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

fn conversation_fingerprint(body: &Value) -> Option<String> {
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

fn without_cache_control(value: &Value) -> Value {
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
pub fn is_origin_only_url(value: &str) -> bool {
    let trimmed = value.trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((_scheme, rest)) => !rest.contains('/'),
        None => !trimmed.contains('/'),
    }
}
    fn build_url(base_url: &str, endpoint: &str) -> String {
        let base_trimmed = base_url.trim_end_matches('/');
        let endpoint_trimmed = endpoint.trim_start_matches('/');

        // OpenAI/Codex 的 base_url 可能是：
        // - 纯 origin: https://api.openai.com  (需要自动补 /v1)
        // - 已含 /v1: https://api.openai.com/v1 (直接拼接)
        // - 自定义前缀: https://xxx/openai (不添加 /v1，直接拼接)

        // 检查 base_url 是否已经包含 /v1
        let already_has_v1 = base_trimmed.ends_with("/v1");
        let origin_only = is_origin_only_url(base_trimmed);

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