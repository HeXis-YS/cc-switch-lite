use anyhow::{bail, Context, Result};
use glob_match::glob_match;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io::Write, net::SocketAddr, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub compat: crate::compat::CompatConfig,
    pub server: Server,
    #[serde(default, skip_serializing_if = "Route::is_empty")]
    pub route: Route,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteRule>,
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_route: Option<DefaultRoute>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Server {
    pub listen: SocketAddr,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:15721".parse().unwrap(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    #[serde(default)]
    pub provider: String,
}

impl Route {
    fn is_empty(&self) -> bool {
        self.provider.is_empty()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRule {
    pub name: String,
    pub model_pattern: String,
    pub provider: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultRoute {
    pub provider: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: String,
    #[serde(default)]
    pub compat: crate::compat::request::ProviderCompat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Responses,
    ChatCompletions,
    Messages,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).context("Cannot read configuration")?;
        // Parser diagnostics can contain the source line, including credentials.
        let config: Self = serde_saphyr::from_str(&text)
            .map_err(|_| anyhow::anyhow!("Invalid YAML configuration"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.compat.validate()?;
        if self.version != 1 {
            bail!("Unsupported configuration version");
        }
        let mut ids = HashSet::new();
        for p in &self.providers {
            p.compat.validate()?;
            if p.id.trim().is_empty() || p.name.trim().is_empty() || !ids.insert(&p.id) {
                bail!("Provider IDs and names must be nonempty; IDs must be unique");
            }
            let url = url::Url::parse(&p.base_url)
                .map_err(|_| anyhow::anyhow!("Invalid provider URL"))?;
            if !matches!(url.scheme(), "http" | "https")
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || (url.query().is_some() && p.compat.meta.is_full_url != Some(true))
                || url.fragment().is_some()
            {
                bail!("Provider URL must be HTTP(S), without credentials, query or fragment");
            }
            if p.api_key.trim().is_empty()
                || reqwest::header::HeaderValue::from_str(&format!("Bearer {}", p.api_key)).is_err()
            {
                bail!("Provider API key must be nonempty and valid for an HTTP header");
            }
        }

        // 验证路由规则
        let mut route_names = HashSet::new();
        for route in &self.routes {
            if route.name.trim().is_empty() || !route_names.insert(&route.name) {
                bail!("Route names must be nonempty and unique");
            }
            if !ids.contains(&route.provider) {
                bail!(
                    "Route '{}' references nonexistent provider '{}'",
                    route.name,
                    route.provider
                );
            }
            if route.model_pattern.trim().is_empty() {
                bail!("Route '{}' has empty model_pattern", route.name);
            }
        }

        // 验证默认路由
        if let Some(default) = &self.default_route {
            if !ids.contains(&default.provider) {
                bail!(
                    "Default route references nonexistent provider '{}'",
                    default.provider
                );
            }
        }

        // 验证遗留单路由（向后兼容）
        if !self.route.provider.is_empty() {
            if !ids.contains(&self.route.provider) {
                bail!("Legacy route provider does not exist");
            }
        } else if self.routes.is_empty() && self.default_route.is_none() {
            bail!("Configuration must have routes, default_route, or legacy route.provider");
        }

        Ok(())
    }

    /// 根据模型名称选择 provider
    pub fn route_for_model(&self, model: &str) -> Option<&Provider> {
        // 1. 尝试匹配路由规则（按顺序，第一个匹配）
        for route in &self.routes {
            if glob_match(&route.model_pattern, model) {
                return self.providers.iter().find(|p| p.id == route.provider);
            }
        }

        // 2. 使用默认路由
        if let Some(default) = &self.default_route {
            return self.providers.iter().find(|p| p.id == default.provider);
        }

        // 3. 回退到遗留单路由（向后兼容）
        if !self.route.provider.is_empty() {
            return self.providers.iter().find(|p| p.id == self.route.provider);
        }

        None
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .context("Cannot create temporary configuration")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        temp.write_all(serde_saphyr::to_string(self)?.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)
            .map_err(|_| anyhow::anyhow!("Cannot replace configuration; original retained"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_route_for_model() {
        let config = Config {
            version: 1,
            compat: Default::default(),
            server: Server::default(),
            route: Route::default(),
            routes: vec![
                RouteRule {
                    name: "claude".into(),
                    model_pattern: "claude-*".into(),
                    provider: "anthropic".into(),
                },
                RouteRule {
                    name: "gpt".into(),
                    model_pattern: "gpt-*".into(),
                    provider: "openai".into(),
                },
                RouteRule {
                    name: "deepseek".into(),
                    model_pattern: "deepseek/*".into(),
                    provider: "openrouter".into(),
                },
            ],
            providers: vec![
                Provider {
                    id: "anthropic".into(),
                    name: "Anthropic".into(),
                    protocol: Protocol::Messages,
                    base_url: "https://api.anthropic.com/v1".into(),
                    api_key: "test-key".into(),
                    compat: Default::default(),
                },
                Provider {
                    id: "openai".into(),
                    name: "OpenAI".into(),
                    protocol: Protocol::Responses,
                    base_url: "https://api.openai.com/v1".into(),
                    api_key: "test-key".into(),
                    compat: Default::default(),
                },
                Provider {
                    id: "openrouter".into(),
                    name: "OpenRouter".into(),
                    protocol: Protocol::ChatCompletions,
                    base_url: "https://openrouter.ai/api/v1".into(),
                    api_key: "test-key".into(),
                    compat: Default::default(),
                },
            ],
            default_route: Some(DefaultRoute {
                provider: "anthropic".into(),
            }),
        };

        // 测试精确前缀匹配
        assert_eq!(
            config
                .route_for_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .id,
            "anthropic"
        );
        assert_eq!(config.route_for_model("gpt-4o").unwrap().id, "openai");

        // 测试路径模式
        assert_eq!(
            config.route_for_model("deepseek/deepseek-chat").unwrap().id,
            "openrouter"
        );

        // 测试默认路由
        assert_eq!(
            config.route_for_model("unknown-model").unwrap().id,
            "anthropic"
        );

        // 测试第一个匹配原则
        let config_overlap = Config {
            version: 1,
            compat: Default::default(),
            server: Server::default(),
            route: Route::default(),
            routes: vec![
                RouteRule {
                    name: "specific".into(),
                    model_pattern: "claude-3-5-*".into(),
                    provider: "openai".into(),
                },
                RouteRule {
                    name: "general".into(),
                    model_pattern: "claude-*".into(),
                    provider: "anthropic".into(),
                },
            ],
            providers: config.providers.clone(),
            default_route: None,
        };

        // 应该匹配第一个更具体的规则
        assert_eq!(
            config_overlap
                .route_for_model("claude-3-5-sonnet-20241022")
                .unwrap()
                .id,
            "openai"
        );

        // 应该匹配第二个规则
        assert_eq!(
            config_overlap.route_for_model("claude-3-opus").unwrap().id,
            "anthropic"
        );
    }
}

#[test]
fn test_route_ccs_model() {
    let config = Config {
        version: 1,
        compat: Default::default(),
        server: Server::default(),
        route: Route::default(),
        routes: vec![RouteRule {
            name: "claude-models".into(),
            model_pattern: "claude-*".into(),
            provider: "openlux-claude".into(),
        }],
        providers: vec![
            Provider {
                id: "openlux-claude".into(),
                name: "OpenLux Claude".into(),
                protocol: Protocol::Messages,
                base_url: "https://api.openlux.ai/v1".into(),
                api_key: "sk-test".into(),
                compat: Default::default(),
            },
            Provider {
                id: "openlux".into(),
                name: "OpenLux".into(),
                protocol: Protocol::Responses,
                base_url: "https://api.openlux.ai/v1".into(),
                api_key: "sk-test".into(),
                compat: Default::default(),
            },
        ],
        default_route: Some(DefaultRoute {
            provider: "openlux".into(),
        }),
    };

    // 测试带前缀的模型名称
    let model = "ccs-openlux-claude/claude-opus-5-5";
    let result = config.route_for_model(model);
    assert_eq!(result.unwrap().id, "openlux");

    // 测试不带前缀的
    let model2 = "claude-opus-5-5";
    let result2 = config.route_for_model(model2);
    assert_eq!(result2.unwrap().id, "openlux-claude");
}
