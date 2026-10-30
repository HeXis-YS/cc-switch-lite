// Derived from cc-switch@214e75ba7f625c679dd4e61df166ec4463245fe7.
// Copyright (c) 2025 Jason Young. MIT; see proxy/providers/LICENSE-cc-switch.
use crate::provider::{CodexChatReasoningConfig, Provider};
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

pub fn should_send_codex_chat_prompt_cache_key(provider: &Provider) -> bool {
    match provider
        .meta
        .as_ref()
        .and_then(|meta| meta.prompt_cache_routing.as_deref())
        .unwrap_or("auto")
    {
        "enabled" => return true,
        "disabled" => return false,
        _ => {}
    }

    let base_url = provider
        .settings_config
        .get("base_url")
        .or_else(|| provider.settings_config.get("baseURL"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            provider
                .settings_config
                .get("config")
                .and_then(|value| value.as_str())
                .and_then(extract_codex_base_url_from_toml)
        });

    let Some(base_url) = base_url else {
        return false;
    };
    let Ok(url) = url::Url::parse(&base_url) else {
        return false;
    };

    match url.host_str() {
        Some("api.openai.com") => true,
        Some("api.kimi.com") => {
            let path = url.path().trim_end_matches('/');
            path == "/coding" || path.starts_with("/coding/")
        }
        _ => false,
    }
}

pub fn inject_codex_chat_prompt_cache_key(
    provider: &Provider,
    chat_body: &mut JsonValue,
    explicit_key: Option<&str>,
    client_session_id: Option<&str>,
) -> bool {
    if !should_send_codex_chat_prompt_cache_key(provider) {
        return false;
    }

    let key = explicit_key
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .or_else(|| {
            client_session_id
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
        });
    let Some(key) = key else {
        return false;
    };

    chat_body["prompt_cache_key"] = JsonValue::String(key.to_string());
    true
}

pub fn resolve_codex_chat_reasoning_config(
    provider: &Provider,
    body: &JsonValue,
) -> Option<CodexChatReasoningConfig> {
    let mut config = if let Some(config) = provider
        .meta
        .as_ref()
        .and_then(|meta| meta.codex_chat_reasoning.clone())
    {
        normalize_codex_chat_reasoning_config(config)
    } else {
        infer_codex_chat_reasoning_config(provider, body)?
    };

    // zen 的合法 effort 档位是逐模型的（models.dev：glm-5.2 仅 high|max、
    // kimi-k3 仅 max、qwen/glm-5.1 等为 toggle 型无 effort），opencode 客户端
    // 也严格按模型声明发值。按请求模型从 modelCatalog 的 reasoningLevels
    // （#6228 引入的逐模型声明）查表附上；查不到（模型未收录 / 条目未声明
    // effort）→ None，转换层将完全不发 reasoning_effort。
    if config.effort_value_mode.as_deref() == Some("zen") {
        config.effort_levels = zen_catalog_effort_levels(provider, body);
    }

    Some(config)
}

fn zen_catalog_effort_levels(provider: &Provider, body: &JsonValue) -> Option<Vec<String>> {
    let model = body.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    let entries = provider
        .settings_config
        .get("modelCatalog")?
        .get("models")?
        .as_array()?;
    let entry = entries.iter().find(|entry| {
        entry
            .get("model")
            .and_then(|value| value.as_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(model))
    })?;
    let levels_value = entry
        .get("reasoningLevels")
        .or_else(|| entry.get("reasoning_levels"))?;
    let levels: Vec<String> = levels_value
        .as_array()?
        .iter()
        .filter_map(|level| level.as_str().map(str::to_string))
        .collect();
    (!levels.is_empty()).then_some(levels)
}

fn normalize_codex_chat_reasoning_config(
    mut config: CodexChatReasoningConfig,
) -> CodexChatReasoningConfig {
    if config.supports_effort.unwrap_or(false) && config.supports_thinking.is_none() {
        config.supports_thinking = Some(true);
    }
    config
}

fn infer_codex_chat_reasoning_config(
    provider: &Provider,
    body: &JsonValue,
) -> Option<CodexChatReasoningConfig> {
    let model = body
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| codex_provider_upstream_model(provider))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let base_url = provider
        .settings_config
        .get("base_url")
        .or_else(|| provider.settings_config.get("baseURL"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            provider
                .settings_config
                .get("config")
                .and_then(|v| v.as_str())
                .and_then(extract_codex_base_url_from_toml)
        })
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = provider.name.to_ascii_lowercase();

    // 平台优先：聚合 / 托管平台的 reasoning 接口由平台的推理框架决定，而非模型官方实现，
    // 因此先按平台标识（仅 name + base_url，不含 model 名）判定并覆盖模型规则。
    if let Some(config) = infer_aggregator_platform_config(&name, &base_url) {
        return Some(config);
    }

    let haystack = format!("{name} {base_url} {model}");

    if haystack.contains("deepseek") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("deepseek".to_string()),
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    // StepFun：官方 reasoning 指南与两站模型页（2026-08-15 盘点）——
    // step-3.5-flash-2603 支持 low/high 两档；step-3.7-flash 支持
    // low/medium/high 三档（官方默认 medium）；其余 step 模型（含无后缀
    // step-3.5-flash）不暴露 effort。2603 沿用 low_high 收敛映射；
    // 3.7-flash 必须 passthrough——套 low_high 会把 medium 塌成 high，
    // 造出 wire 上无差异的假档位。全系无思考开关（thinking_param 恒 none）。
    // 第二个 OR 分支覆盖「经中转/聚合跑该模型、但平台 name/base_url 不含 stepfun」的情况。
    if haystack.contains("stepfun") || haystack.contains("step-3.5-flash-2603") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(model.contains("2603") || model.contains("step-3.7-flash")),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some(
                if model.contains("2603") {
                    "low_high"
                } else {
                    "passthrough"
                }
                .to_string(),
            ),
            output_format: Some("reasoning".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("kimi") || haystack.contains("moonshot") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("glm") || haystack.contains("zhipu") || haystack.contains("z.ai") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("qwen") || haystack.contains("dashscope") || haystack.contains("bailian") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("minimax") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("reasoning_split".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_details".to_string()),
            effort_levels: None,
        });
    }

    if haystack.contains("mimo") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    None
}

fn infer_aggregator_platform_config(
    name: &str,
    base_url: &str,
) -> Option<CodexChatReasoningConfig> {
    let platform = format!("{name} {base_url}");

    // OpenRouter：用原生归一化对象 `reasoning: { effort }`（由 OpenRouter 翻译成各底层
    // 模型的正确推理参数，比顶层 OpenAI 别名 reasoning_effort 覆盖面更全）。effort 走
    // "openrouter" 值映射：枚举为 xhigh|high|medium|low|minimal，无 max——max 会触发
    // `400 reasoning_effort: Invalid option`（见 openclaw#77350），故钳到 xhigh。
    // 安全降级：不发 `thinking:{type}`（OpenRouter 不认该字段），避免误配导致请求被拒。
    if platform.contains("openrouter") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(false),
            supports_effort: Some(true),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning.effort".to_string()),
            effort_value_mode: Some("openrouter".to_string()),
            output_format: Some("auto".to_string()),
            effort_levels: None,
        });
    }

    // SiliconFlow：平台级统一 `enable_thinking`，思维回传 reasoning_content。
    // 安全降级：不按 reasoning_effort 发 effort（平台用 thinking_budget 控制深度，
    // 发 reasoning_effort 反而可能不被接受）。
    if platform.contains("siliconflow") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    // ModelScope 魔搭 API-Inference：与 SiliconFlow 同构——平台级统一
    // `enable_thinking` 布尔（官方模型页范例 extra_body {"enable_thinking": bool}，
    // OpenAI SDK 的 extra_body 合并进请求体顶层），思维回传 reasoning_content。
    // 智谱风格 thinking:{type} 是模型厂商自家方言，平台文档零出现——没有这条
    // 分支时挂 GLM 的 ModelScope 供应商会被下方 glm 模型规则错误注入该形态。
    if platform.contains("modelscope") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(false),
            thinking_param: Some("enable_thinking".to_string()),
            effort_param: Some("none".to_string()),
            effort_value_mode: None,
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    // OpenCode Zen（opencode.ai 网关，issue #6112）：其自家客户端对该传输发顶层
    // `reasoning_effort`（provider/transform.ts），平台归一参数；不发厂商原生
    // thinking 形状（glm 模型走 zen 时套智谱 thinking:{type} 网关不认）。
    // 合法档位逐模型（models.dev 的 reasoning_options，opencode 客户端同样严格
    // 按模型声明发值）：具体档位表见供应商 modelCatalog 各条目的 reasoningLevels，
    // 代理由此按请求模型查表钳制（resolve 处附上 effort_levels），无表不发字段。
    // 匹配域名而非裸 "opencode"，避免误伤名字含 opencode 的无关供应商。
    if platform.contains("opencode.ai") {
        return Some(CodexChatReasoningConfig {
            supports_thinking: Some(true),
            supports_effort: Some(true),
            thinking_param: Some("none".to_string()),
            effort_param: Some("reasoning_effort".to_string()),
            effort_value_mode: Some("zen".to_string()),
            output_format: Some("reasoning_content".to_string()),
            effort_levels: None,
        });
    }

    None
}

fn extract_codex_wire_api_from_toml(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<TomlValue>().ok()?;

    if let Some(active_provider) = doc.get("model_provider").and_then(|v| v.as_str()) {
        if let Some(wire_api) = doc
            .get("model_providers")
            .and_then(|providers| providers.get(active_provider))
            .and_then(|provider| provider.get("wire_api"))
            .and_then(|v| v.as_str())
        {
            return Some(wire_api.to_string());
        }
    }

    doc.get("wire_api")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn extract_codex_model_from_toml(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<TomlValue>().ok()?;

    doc.get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
}

pub fn is_origin_only_url(value: &str) -> bool {
    let trimmed = value.trim_end_matches('/');
    match trimmed.split_once("://") {
        Some((_scheme, rest)) => !rest.contains('/'),
        None => !trimmed.contains('/'),
    }
}

pub fn is_codex_responses_endpoint(endpoint: &str) -> bool {
    let path = endpoint
        .split_once('?')
        .map_or(endpoint, |(path, _query)| path);

    matches!(
        path,
        "/responses" | "/v1/responses" | "/responses/compact" | "/v1/responses/compact"
    )
}

pub fn extract_codex_base_url_from_toml(config_text: &str) -> Option<String> {
    let doc = config_text.parse::<toml::Value>().ok()?;

    if let Some(active_provider) = doc.get("model_provider").and_then(|v| v.as_str()) {
        if let Some(base_url) = doc
            .get("model_providers")
            .and_then(|providers| providers.get(active_provider))
            .and_then(|provider| provider.get("base_url"))
            .and_then(|v| v.as_str())
        {
            return Some(base_url.to_string());
        }
    }

    doc.get("base_url")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

pub fn codex_provider_upstream_model(provider: &Provider) -> Option<String> {
    provider
        .settings_config
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            provider
                .settings_config
                .get("config")
                .and_then(|v| v.as_str())
                .and_then(extract_codex_model_from_toml)
        })
}

pub(super) fn provider_is_xai_native_responses(provider: &Provider) -> bool {
    let config_text = provider
        .settings_config
        .get("config")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let Some(wire_api) = extract_codex_wire_api_from_toml(config_text) else {
        return false;
    };
    if !wire_api.eq_ignore_ascii_case("responses") {
        return false;
    }

    extract_codex_base_url_from_toml(config_text)
        .map(|url| url.to_ascii_lowercase())
        .is_some_and(|url| url.contains("api.x.ai"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn create_provider(config: serde_json::Value) -> Provider {
        Provider {
            id: "test".to_string(),
            name: "Test Codex".to_string(),
            settings_config: config,
            meta: None,
        }
    }
    #[test]
    fn prompt_cache_routing_auto_enables_known_upstreams_only() {
        let kimi = create_provider(json!({
            "config": r#"
model_provider = "custom"
[model_providers.custom]
base_url = "https://api.kimi.com/coding/v1"
wire_api = "responses"
"#
        }));
        let openai = create_provider(json!({
            "base_url": "https://api.openai.com/v1"
        }));
        let unknown = create_provider(json!({
            "base_url": "https://strict.example.com/v1"
        }));

        assert!(should_send_codex_chat_prompt_cache_key(&kimi));
        assert!(should_send_codex_chat_prompt_cache_key(&openai));
        assert!(!should_send_codex_chat_prompt_cache_key(&unknown));
    }
    #[test]
    fn prompt_cache_routing_user_override_wins_over_auto_detection() {
        let mut kimi = create_provider(json!({
            "base_url": "https://api.kimi.com/coding/v1"
        }));
        kimi.meta = Some(crate::provider::ProviderMeta {
            prompt_cache_routing: Some("disabled".to_string()),
            ..Default::default()
        });
        assert!(!should_send_codex_chat_prompt_cache_key(&kimi));

        let mut unknown = create_provider(json!({
            "base_url": "https://strict.example.com/v1"
        }));
        unknown.meta = Some(crate::provider::ProviderMeta {
            prompt_cache_routing: Some("enabled".to_string()),
            ..Default::default()
        });
        assert!(should_send_codex_chat_prompt_cache_key(&unknown));
    }
    #[test]
    fn prompt_cache_key_prefers_explicit_key_then_real_session() {
        let provider = create_provider(json!({
            "base_url": "https://api.kimi.com/coding/v1"
        }));
        let mut explicit_body = json!({ "model": "kimi-for-coding" });
        assert!(inject_codex_chat_prompt_cache_key(
            &provider,
            &mut explicit_body,
            Some("request-key"),
            Some("session-key"),
        ));
        assert_eq!(explicit_body["prompt_cache_key"], "request-key");

        let mut session_body = json!({ "model": "kimi-for-coding" });
        assert!(inject_codex_chat_prompt_cache_key(
            &provider,
            &mut session_body,
            None,
            Some("session-key"),
        ));
        assert_eq!(session_body["prompt_cache_key"], "session-key");
    }
    #[test]
    fn prompt_cache_key_is_not_injected_without_real_session_or_support() {
        let kimi = create_provider(json!({
            "base_url": "https://api.kimi.com/coding/v1"
        }));
        let mut no_session_body = json!({ "model": "kimi-for-coding" });
        assert!(!inject_codex_chat_prompt_cache_key(
            &kimi,
            &mut no_session_body,
            None,
            None,
        ));
        assert!(no_session_body.get("prompt_cache_key").is_none());

        let unknown = create_provider(json!({
            "base_url": "https://strict.example.com/v1"
        }));
        let mut unsupported_body = json!({ "model": "other" });
        assert!(!inject_codex_chat_prompt_cache_key(
            &unknown,
            &mut unsupported_body,
            Some("request-key"),
            Some("session-key"),
        ));
        assert!(unsupported_body.get("prompt_cache_key").is_none());
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_infers_deepseek_effort_support() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "deepseek"
model = "deepseek-v4-pro"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "chat"
"#
        }));

        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "deepseek-v4-pro" }))
                .unwrap();

        assert_eq!(config.supports_thinking, Some(true));
        assert_eq!(config.supports_effort, Some(true));
        assert_eq!(config.effort_value_mode.as_deref(), Some("deepseek"));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_explicit_meta_overrides_inference() {
        let mut provider = create_provider(json!({
            "config": r#"
model_provider = "deepseek"
model = "deepseek-v4-pro"

[model_providers.deepseek]
name = "DeepSeek"
base_url = "https://api.deepseek.com"
wire_api = "chat"
"#
        }));
        provider.meta = Some(crate::provider::ProviderMeta {
            codex_chat_reasoning: Some(CodexChatReasoningConfig {
                supports_thinking: Some(false),
                supports_effort: Some(false),
                thinking_param: Some("none".to_string()),
                effort_param: Some("none".to_string()),
                effort_value_mode: None,
                output_format: Some("auto".to_string()),
                effort_levels: None,
            }),
            ..Default::default()
        });

        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "deepseek-v4-pro" }))
                .unwrap();

        assert_eq!(config.supports_thinking, Some(false));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.thinking_param.as_deref(), Some("none"));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_openrouter_platform_overrides_model() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "openrouter"
model = "deepseek/deepseek-chat-v3.1"

[model_providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.ai/api/v1"
wire_api = "chat"
"#
        }));

        // 模型名含 "deepseek"，但平台是 OpenRouter —— 平台规则必须覆盖模型规则。
        let config = resolve_codex_chat_reasoning_config(
            &provider,
            &json!({ "model": "deepseek/deepseek-chat-v3.1" }),
        )
        .unwrap();

        assert_eq!(config.thinking_param.as_deref(), Some("none"));
        assert_eq!(config.effort_param.as_deref(), Some("reasoning.effort"));
        assert_eq!(config.effort_value_mode.as_deref(), Some("openrouter"));
        assert_eq!(config.supports_effort, Some(true));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_siliconflow_platform_overrides_minimax() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "siliconflow"
model = "MiniMaxAI/MiniMax-M2.5"

[model_providers.siliconflow]
name = "SiliconFlow"
base_url = "https://api.siliconflow.cn/v1"
wire_api = "chat"
"#
        }));

        // 模型是 MiniMax（官方用 reasoning_split），但平台是 SiliconFlow —— 应走平台的 enable_thinking。
        let config = resolve_codex_chat_reasoning_config(
            &provider,
            &json!({ "model": "MiniMaxAI/MiniMax-M2.5" }),
        )
        .unwrap();

        assert_eq!(config.thinking_param.as_deref(), Some("enable_thinking"));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.output_format.as_deref(), Some("reasoning_content"));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_modelscope_platform_overrides_glm() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "modelscope"
model = "ZhipuAI/GLM-5.2"

[model_providers.modelscope]
name = "ModelScope"
base_url = "https://api-inference.modelscope.cn/v1"
wire_api = "chat"
"#
        }));

        // 模型是 GLM（智谱自家用 thinking:{type}），但平台是 ModelScope ——
        // 应走平台级 enable_thinking，而不是被 glm 模型规则注入智谱方言。
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "ZhipuAI/GLM-5.2" }))
                .unwrap();

        assert_eq!(config.thinking_param.as_deref(), Some("enable_thinking"));
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.output_format.as_deref(), Some("reasoning_content"));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_opencode_zen_platform_overrides_model_vendor() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "opencode"
model = "glm-5.2"

[model_providers.opencode]
name = "OpenCode Go"
base_url = "https://opencode.ai/zen/go/v1"
wire_api = "chat"
"#
        }));

        // 模型是 GLM（智谱自家用 thinking:{type} 方言），但平台是 OpenCode Zen ——
        // 必须走平台配置：顶层 reasoning_effort + reasoning_content，
        // 不得注入智谱 thinking 形状。
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "glm-5.2" })).unwrap();

        assert_eq!(config.supports_thinking, Some(true));
        assert_eq!(config.supports_effort, Some(true));
        assert_eq!(config.thinking_param.as_deref(), Some("none"));
        assert_eq!(config.effort_param.as_deref(), Some("reasoning_effort"));
        assert_eq!(config.effort_value_mode.as_deref(), Some("zen"));
        assert_eq!(config.output_format.as_deref(), Some("reasoning_content"));
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_zen_attaches_per_model_effort_levels() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "opencode"
model = "glm-5.2"

[model_providers.opencode]
name = "OpenCode Go"
base_url = "https://opencode.ai/zen/go/v1"
wire_api = "chat"
"#,
            "modelCatalog": {
                "models": [
                    { "model": "glm-5.2", "reasoningLevels": ["high", "max"] },
                    { "model": "deepseek-v4-flash", "reasoningLevels": ["low", "high", "max"] },
                    { "model": "glm-5.1" }
                ]
            }
        }));

        // 逐模型查表：声明了 reasoningLevels 的模型附上各自档位（模型名大小写不敏感）。
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "GLM-5.2" })).unwrap();
        assert_eq!(
            config.effort_levels,
            Some(vec!["high".to_string(), "max".to_string()])
        );

        let config = resolve_codex_chat_reasoning_config(
            &provider,
            &json!({ "model": "deepseek-v4-flash" }),
        )
        .unwrap();
        assert_eq!(
            config.effort_levels,
            Some(vec![
                "low".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );

        // toggle 型模型（目录条目未声明 reasoningLevels）→ None：转换层不发 effort 字段。
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "glm-5.1" })).unwrap();
        assert_eq!(config.effort_value_mode.as_deref(), Some("zen"));
        assert!(config.effort_levels.is_none());

        // 目录未收录的模型 → 同样 None。
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "kimi-k3" })).unwrap();
        assert!(config.effort_levels.is_none());
    }
    #[test]
    fn test_resolve_codex_chat_reasoning_zen_levels_attach_on_explicit_meta_too() {
        let mut provider = create_provider(json!({
            "config": r#"
model_provider = "opencode"
model = "deepseek-v4-flash"

[model_providers.opencode]
name = "OpenCode Go"
base_url = "https://opencode.ai/zen/go/v1"
wire_api = "chat"
"#,
            "modelCatalog": {
                "models": [
                    { "model": "deepseek-v4-flash", "reasoning_levels": ["low", "high", "max"] }
                ]
            }
        }));
        // 显式 meta（表单手选 zen 模式）同样要在 resolve 末端按请求模型附表；
        // 且手写/旧数据可能是 snake_case 的 reasoning_levels（加载侧双格式兼容）。
        provider.meta = Some(crate::provider::ProviderMeta {
            codex_chat_reasoning: Some(CodexChatReasoningConfig {
                supports_thinking: Some(true),
                supports_effort: Some(true),
                thinking_param: Some("none".to_string()),
                effort_param: Some("reasoning_effort".to_string()),
                effort_value_mode: Some("zen".to_string()),
                output_format: Some("reasoning_content".to_string()),
                effort_levels: None,
            }),
            ..Default::default()
        });

        let config = resolve_codex_chat_reasoning_config(
            &provider,
            &json!({ "model": "deepseek-v4-flash" }),
        )
        .unwrap();

        assert_eq!(config.effort_value_mode.as_deref(), Some("zen"));
        assert_eq!(
            config.effort_levels,
            Some(vec![
                "low".to_string(),
                "high".to_string(),
                "max".to_string()
            ])
        );
    }
    #[test]
    fn test_infer_codex_chat_reasoning_stepfun_per_model_effort() {
        let provider = create_provider(json!({
            "config": r#"
model_provider = "stepfun"
model = "step-3.7-flash"

[model_providers.stepfun]
name = "StepFun"
base_url = "https://api.stepfun.com/step_plan/v1"
wire_api = "chat"
"#
        }));

        // step-3.7-flash：官方三档 low/medium/high —— 必须 passthrough，
        // 套 low_high 会把 medium 塌成 high（假差异档）
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "step-3.7-flash" }))
                .unwrap();
        assert_eq!(config.supports_effort, Some(true));
        assert_eq!(config.effort_value_mode.as_deref(), Some("passthrough"));

        // step-3.5-flash-2603：官方两档 low/high，沿用收敛映射
        let config = resolve_codex_chat_reasoning_config(
            &provider,
            &json!({ "model": "step-3.5-flash-2603" }),
        )
        .unwrap();
        assert_eq!(config.supports_effort, Some(true));
        assert_eq!(config.effort_value_mode.as_deref(), Some("low_high"));

        // 无后缀 step-3.5-flash：官方未暴露 effort，不下发
        let config =
            resolve_codex_chat_reasoning_config(&provider, &json!({ "model": "step-3.5-flash" }))
                .unwrap();
        assert_eq!(config.supports_effort, Some(false));
        assert_eq!(config.thinking_param.as_deref(), Some("none"));
    }
}
