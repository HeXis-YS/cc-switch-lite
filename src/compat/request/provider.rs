//! Provider configuration projection for the local request pipeline.
use crate::{
    config::{Protocol, Provider},
    provider::{CodexChatReasoningConfig, LocalProxyRequestOverrides},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct CompatMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_chat_reasoning: Option<CodexChatReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_routing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub impersonate_claude_code: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_full_url: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_proxy_request_overrides: Option<LocalProxyRequestOverrides>,
}
fn empty_object() -> Value {
    json!({})
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderCompat {
    pub meta: CompatMeta,
    #[serde(default = "empty_object")]
    pub settings_config: Value,
}
impl Default for ProviderCompat {
    fn default() -> Self {
        Self {
            meta: CompatMeta::default(),
            settings_config: empty_object(),
        }
    }
}

/// Supply the same upstream facts to copied code without allowing a second URL,
/// authentication source or model routing configuration.
pub fn upstream_provider(provider: &Provider) -> crate::provider::Provider {
    let mut settings = provider.compat.settings_config.clone();
    settings["base_url"] = json!(provider.base_url);
    let wire = match provider.protocol {
        Protocol::Responses => "responses",
        Protocol::ChatCompletions => "chat",
        Protocol::Messages => "anthropic",
    };
    settings["config"] = json!(format!(
        "base_url = {}\nwire_api = {}\n",
        toml::Value::String(provider.base_url.clone()),
        toml::Value::String(wire.into())
    ));
    crate::provider::Provider {
        id: provider.id.clone(),
        name: provider.name.clone(),
        settings_config: settings,
        meta: Some(provider.compat.meta.clone()),
    }
}
