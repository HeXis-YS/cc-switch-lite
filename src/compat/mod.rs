//! API-key compatibility adapter around the pinned cc-switch protocol chain.
pub mod request;
pub mod response;
pub mod response_headers;
pub mod response_helpers;
pub mod retry;
pub mod secrets;

use crate::proxy::types::{OptimizerConfig, RectifierConfig};
use serde::{Deserialize, Serialize};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompatConfig {
    pub optimizer: OptimizerConfig,
    pub rectifier: RectifierConfig,
    pub transport: TransportConfig,
}
/// Upstream disables these timeouts when failover is off. Nonzero values are
/// rejected rather than silently ignored in the single-provider application.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TransportConfig {
    pub streaming_first_byte_timeout: u32,
    pub streaming_idle_timeout: u32,
    pub non_streaming_timeout: u32,
}
impl CompatConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.transport.streaming_first_byte_timeout != 0
            || self.transport.streaming_idle_timeout != 0
            || self.transport.non_streaming_timeout != 0
        {
            anyhow::bail!("Transport timeouts require failover in upstream and are disabled in single-provider mode");
        }
        Ok(())
    }
}

mod validation;
