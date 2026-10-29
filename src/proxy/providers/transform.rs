// Derived from cc-switch, Copyright (c) 2025 Jason Young. MIT License.
// See LICENSE-cc-switch in this directory.
use serde_json::{json, Value};

/// 部分 OpenAI 兼容网关（ClinePass，#5658）把非流式 Chat Completions 响应包成
/// `{"data": {...}, "success": true}`。只认顶层没有 `choices` 数组、`data` 里有的形状，
/// 标准响应不受影响。
fn is_chat_completion_envelope(body: &Value) -> bool {
    !body.get("choices").is_some_and(Value::is_array)
        && body
            .get("data")
            .and_then(|data| data.get("choices"))
            .is_some_and(Value::is_array)
}

/// 把包在 `data` 里的 Chat Completions 响应解开；顶层的非 null 字段优先于 `data`
/// 里的同名字段。不是这种形状时原样返回。
pub(crate) fn unwrap_chat_completion_envelope(body: Value) -> Value {
    if !is_chat_completion_envelope(&body) {
        return body;
    }
    let Value::Object(mut outer) = body else {
        unreachable!("envelope check requires an object");
    };
    let Some(Value::Object(mut completion)) = outer.remove("data") else {
        unreachable!("envelope check requires an object `data`");
    };
    for (key, value) in outer {
        // 顶层 `choices` 此时只可能是非数组的占位值，不能盖掉 `data` 里的真值
        if key != "choices" && !value.is_null() {
            completion.insert(key, value);
        }
    }
    Value::Object(completion)
}

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
    fn test_unwrap_chat_completion_envelope_prefers_top_level_fields() {
        let input = json!({
            "id": "top-id",
            "usage": null,
            "choices": null,
            "data": {
                "id": "inner-id",
                "model": "inner-model",
                "choices": [{"message": {"role": "assistant", "content": "x"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 2}
            }
        });

        let result = unwrap_chat_completion_envelope(input);
        assert_eq!(result["id"], "top-id");
        assert_eq!(result["model"], "inner-model");
        assert_eq!(result["usage"]["prompt_tokens"], 3);
        assert!(result["choices"].is_array());
        assert!(result.get("data").is_none());
    }

    #[test]
    fn test_unwrap_chat_completion_envelope_leaves_other_shapes_alone() {
        let standard = json!({
            "id": "chatcmpl-1",
            "choices": [{"message": {"role": "assistant", "content": "hi"}}],
            "data": {"choices": [{"message": {"role": "assistant", "content": "other"}}]}
        });
        assert_eq!(unwrap_chat_completion_envelope(standard.clone()), standard);

        let unrelated = json!({"data": {"text": "no choices"}});
        assert_eq!(
            unwrap_chat_completion_envelope(unrelated.clone()),
            unrelated
        );
    }

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
