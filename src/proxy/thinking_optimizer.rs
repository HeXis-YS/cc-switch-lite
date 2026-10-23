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
    ["fable-5", "mythos-5", "fable-5-1"]
        .iter()
        .any(|needle| normalized.contains(needle))
        || is_opus_5_5(model)
        || is_sonnet_5_5(model)
}

/// Opus 5.5 / Sonnet 5.5 / Fable 5.1 require adaptive thinking, but accept tool 
/// history without signed thinking and reject forced `any`/`tool` selection.
/// Keep this compatibility scoped to the verified generation rather than changing legacy models.
pub(crate) fn is_opus_5_5(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    normalized
        .split_once("opus-5-5")
        .is_some_and(|(_, suffix)| suffix.is_empty() || suffix.starts_with('-'))
}

pub(crate) fn is_sonnet_5_5(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    normalized
        .split_once("sonnet-5-5")
        .is_some_and(|(_, suffix)| suffix.is_empty() || suffix.starts_with('-'))
}

pub(crate) fn is_fable_5_1(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    normalized
        .split_once("fable-5-1")
        .is_some_and(|(_, suffix)| suffix.is_empty() || suffix.starts_with('-'))
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
        
        // Legacy models
        assert!(thinking_cannot_be_disabled("claude-fable-5"));
        assert!(!thinking_cannot_be_disabled("claude-sonnet-5"));
        
        // 5.5 generation models (verified)
        assert!(thinking_cannot_be_disabled("claude-opus-5-5"));
        assert!(thinking_cannot_be_disabled("claude-opus-5.5"));
        assert!(thinking_cannot_be_disabled("claude-sonnet-5-5"));
        assert!(thinking_cannot_be_disabled("claude-sonnet-5.5"));
        assert!(thinking_cannot_be_disabled("claude-fable-5-1"));
        
        assert!(is_opus_5_5("claude-opus-5-5"));
        assert!(is_opus_5_5("claude-opus-5.5"));
        assert!(is_opus_5_5("anthropic/claude-opus-5.5"));
        assert!(!is_opus_5_5("claude-opus-5"));
        
        assert!(is_sonnet_5_5("claude-sonnet-5-5"));
        assert!(is_sonnet_5_5("claude-sonnet-5.5"));
        assert!(!is_sonnet_5_5("claude-sonnet-5"));
        
        assert!(is_fable_5_1("claude-fable-5-1"));
        assert!(is_fable_5_1("claude-fable-5.1"));
        assert!(!is_fable_5_1("claude-fable-5"));
    }
}
