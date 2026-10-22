// Derived from cc-switch, Copyright (c) 2025 Jason Young. MIT License.
// See LICENSE-cc-switch in this directory.
use serde_json::{json, Value};

pub fn is_openai_o_series(model: &str) -> bool {
    model.len() > 1
        && model.starts_with('o')
        && model.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit())
}

pub fn supports_reasoning_effort(model: &str) -> bool {
    let normalized = model.to_lowercase();
    is_openai_o_series(&normalized)
        || normalized
            .strip_prefix("gpt-")
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_digit() && c >= '5')
        || normalized
            .strip_prefix("grok-4.")
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|minor| minor.parse::<u32>().ok())
            .is_some_and(|minor| minor >= 5)
        || normalized.starts_with("grok-build-")
}

pub(crate) fn inject_openai_stream_include_usage(result: &mut Value) {
    let is_stream = result
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_stream {
        return;
    }
    match result.get_mut("stream_options") {
        Some(Value::Object(opts)) => {
            opts.insert("include_usage".to_string(), json!(true));
        }
        _ => {
            result["stream_options"] = json!({ "include_usage": true });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_openai_o_series() {
        assert!(is_openai_o_series("o1"));
        assert!(is_openai_o_series("o1-preview"));
        assert!(is_openai_o_series("o1-mini"));
        assert!(is_openai_o_series("o3"));
        assert!(is_openai_o_series("o3-mini"));
        assert!(is_openai_o_series("o4-mini"));
        assert!(!is_openai_o_series("gpt-4o"));
        assert!(!is_openai_o_series("openai-gpt"));
        assert!(!is_openai_o_series("o"));
        assert!(!is_openai_o_series(""));
    }

    #[test]
    fn test_supports_reasoning_effort() {
        assert!(supports_reasoning_effort("o1"));
        assert!(supports_reasoning_effort("o3-mini"));
        assert!(supports_reasoning_effort("gpt-5"));
        assert!(supports_reasoning_effort("gpt-5.4"));
        assert!(supports_reasoning_effort("gpt-5-codex"));
        assert!(supports_reasoning_effort("grok-4.5"));
        assert!(supports_reasoning_effort("grok-4.6"));
        assert!(supports_reasoning_effort("grok-4.6-build"));
        assert!(supports_reasoning_effort("grok-build-0.1"));
        // The rule covers the whole grok-4.x (x >= 5) family, so future
        // releases need no whitelist update.
        assert!(supports_reasoning_effort("grok-4.7"));
        assert!(supports_reasoning_effort("grok-4.7-build"));
        assert!(supports_reasoning_effort("grok-4.10"));
        assert!(supports_reasoning_effort("grok-4.10-build"));
        assert!(supports_reasoning_effort("GROK-4.10-BUILD"));
        assert!(!supports_reasoning_effort("grok-4."));
        assert!(!supports_reasoning_effort("grok-4.build"));
        assert!(!supports_reasoning_effort("grok-4.4"));
        assert!(!supports_reasoning_effort("gpt-4o"));
        assert!(!supports_reasoning_effort("claude-sonnet-4-6"));
        assert!(!supports_reasoning_effort("grok-4"));
    }
}
