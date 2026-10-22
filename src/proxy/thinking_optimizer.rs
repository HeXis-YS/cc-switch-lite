// Derived from cc-switch, Copyright (c) 2025 Jason Young. MIT License.
// See LICENSE-cc-switch in providers/.
pub(crate) fn uses_adaptive_thinking(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    [
        "fable-5",
        "mythos-5",
        "mythos-preview",
        "sonnet-5",
        "opus-5",
        "opus-4-8",
        "opus-4-7",
        "opus-4-6",
        "sonnet-4-6",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub(crate) fn adaptive_thinking_is_default(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    [
        "fable-5",
        "mythos-5",
        "mythos-preview",
        "opus-5",
        "sonnet-5",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

pub(crate) fn thinking_cannot_be_disabled(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    ["fable-5", "mythos-5"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn normalize_model_name(model: &str) -> String {
    model.trim().to_ascii_lowercase().replace(['.', '_'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_generation_models_use_adaptive_thinking() {
        for model in [
            "claude-sonnet-5",
            "anthropic/claude-fable-5",
            "claude-mythos-5",
            "claude-opus-5",
            "claude-opus-4.8",
        ] {
            assert!(uses_adaptive_thinking(model), "model={model}");
        }
        for model in [
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ] {
            assert!(adaptive_thinking_is_default(model), "model={model}");
        }
        for model in ["claude-opus-4-8", "claude-opus-4-5", "claude-sonnet-4-6"] {
            assert!(!adaptive_thinking_is_default(model), "model={model}");
        }
        assert!(thinking_cannot_be_disabled("claude-fable-5"));
        assert!(!thinking_cannot_be_disabled("claude-sonnet-5"));
    }
}
