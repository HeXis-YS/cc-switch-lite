use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io::Write, net::SocketAddr, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub compat: crate::compat::CompatConfig,
    pub server: Server,
    pub route: Route,
    pub providers: Vec<Provider>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
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
        let config: Self =
            toml::from_str(&text).map_err(|_| anyhow::anyhow!("Invalid TOML configuration"))?;
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
        if !ids.contains(&self.route.provider) {
            bail!("Current route provider does not exist");
        }
        Ok(())
    }
    pub fn selected(&self) -> &Provider {
        self.providers
            .iter()
            .find(|p| p.id == self.route.provider)
            .expect("validated configuration")
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
        temp.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)
            .map_err(|_| anyhow::anyhow!("Cannot replace configuration; original retained"))?;
        Ok(())
    }
}
