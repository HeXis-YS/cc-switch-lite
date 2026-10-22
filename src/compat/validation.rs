use super::request::ProviderCompat;
use anyhow::{bail, Result};
impl ProviderCompat {
    pub fn validate(&self) -> Result<()> {
        let Some(settings) = self.settings_config.as_object() else {
            bail!("compat.settings_config must be a table");
        };
        for key in settings.keys() {
            if !matches!(key.as_str(), "modelCatalog" | "models") {
                bail!("Unsupported compatibility setting; URL, authentication and model mapping belong to the base configuration or are outside scope");
            }
        }
        if let Some(route) = self.meta.prompt_cache_routing.as_deref() {
            if !matches!(route, "auto" | "enabled" | "disabled") {
                bail!("Invalid promptCacheRouting");
            }
        }
        if let Some(ua) = &self.meta.custom_user_agent {
            if http::HeaderValue::from_str(ua).is_err() {
                bail!("Invalid customUserAgent");
            }
        }
        if let Some(overrides) = &self.meta.local_proxy_request_overrides {
            if let Some(body) = &overrides.body {
                let Some(map) = body.as_object() else {
                    bail!("Compatibility body overrides must be a table");
                };
                if map.contains_key("model") {
                    bail!("Model overrides are outside single-provider compatibility scope");
                }
            }
            for (key, value) in &overrides.headers {
                let name = http::HeaderName::from_bytes(key.as_bytes())
                    .map_err(|_| anyhow::anyhow!("Invalid override header"))?;
                if matches!(
                    name.as_str(),
                    "authorization"
                        | "x-api-key"
                        | "x-goog-api-key"
                        | "host"
                        | "proxy-authorization"
                        | "content-length"
                        | "transfer-encoding"
                ) {
                    bail!("Authentication, destination and framing headers cannot be overridden");
                }
                http::HeaderValue::from_str(value)
                    .map_err(|_| anyhow::anyhow!("Invalid override header value"))?;
            }
        }
        Ok(())
    }
}
