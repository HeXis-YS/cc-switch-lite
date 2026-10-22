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
