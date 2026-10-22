//! Independent assembly of the pinned Codex forwarder request-body stages.
//! Branch ordering follows upstream forwarder.rs 1820-2070. This fixture adapter
//! deliberately accepts explicit test facts rather than production config types.
use crate::proxy::{
    body_filter::filter_private_params_with_whitelist, json_canonical::canonicalize_value,
    providers as p,
};
use serde_json::Value;

#[derive(Default)]
pub struct Facts<'a> {
    pub protocol: &'a str,
    pub base_url: &'a str,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<crate::provider::CodexChatReasoningConfig>,
    pub cache_injection: bool,
}

pub async fn request_body(
    mut request: Value,
    facts: Facts<'_>,
    history: &p::codex_chat_history::CodexChatHistoryStore,
) -> Result<Value, crate::proxy::error::ProxyError> {
    let mut body = match facts.protocol {
        "chat" => {
            history.enrich_request(&mut request).await;
            let mut result = p::transform_codex_chat::responses_to_chat_completions_with_reasoning(
                request,
                facts.reasoning.as_ref(),
            )?;
            p::codex_chat_common::fill_gemini_thought_signature_fallback(&mut result);
            result
        }
        "messages" => {
            if let Some(limit) = facts.max_output_tokens.filter(|n| *n > 0) {
                request["max_output_tokens"] = Value::from(limit);
            }
            let mut result =
                p::transform_codex_anthropic::responses_request_to_anthropic(request, 8192)?;
            if let Some(model) = result.get("model").and_then(Value::as_str) {
                let trimmed = model.trim_end();
                let bytes = trimmed.as_bytes();
                if bytes.len() >= 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b"[1m]") {
                    result["model"] = Value::String(trimmed[..trimmed.len() - 4].trim_end().into());
                }
            }
            crate::proxy::cache_injector::inject(
                &mut result,
                &crate::proxy::types::OptimizerConfig {
                    enabled: true,
                    thinking_optimizer: false,
                    cache_injection: facts.cache_injection,
                },
            );
            result
        }
        "responses" => {
            p::codex_compaction::prepare_native_third_party_request(&mut request);
            request
        }
        other => panic!("Unsupported oracle fixture protocol: {other}"),
    };
    if facts.protocol == "chat"
        && p::transform_codex_chat_moonshot_schema::upstream_requires_ref_sibling_all_of(
            facts.base_url,
        )
    {
        p::transform_codex_chat_moonshot_schema::wrap_ref_siblings_in_chat_tools(&mut body);
    }
    Ok(canonicalize_value(filter_private_params_with_whitelist(
        body,
        &[],
    )))
}
