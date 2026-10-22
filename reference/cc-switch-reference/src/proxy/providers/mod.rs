pub mod codex_chat_common;
pub mod codex_chat_history;
pub mod codex_chat_tool_history;
pub mod codex_compaction;
pub mod codex_responses_sse;
pub mod inline_think;
pub mod streaming_codex_anthropic;
pub mod streaming_codex_chat;
pub mod transform;
pub mod transform_codex_anthropic;
pub mod transform_codex_chat;
pub mod transform_responses;
pub mod reasoning_bridge;
pub mod transform_codex_chat_moonshot_schema;
pub mod transform_codex_responses_namespace;
pub mod transform_codex_responses_xai_sanitize;

/// Exact endpoint predicate from upstream proxy/providers/codex.rs.
pub fn is_codex_responses_endpoint(endpoint: &str) -> bool {
    let path = endpoint.split_once('?').map_or(endpoint, |(path, _query)| path);
    matches!(path, "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact")
}
