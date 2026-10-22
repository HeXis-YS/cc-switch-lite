//! Independently pinned protocol oracle. No dependency on cc-switch-lite.
#![allow(dead_code, private_interfaces)]
pub mod provider;
pub mod proxy;
pub mod request_oracle;
pub mod header_oracle;

pub mod providers { pub use crate::proxy::providers::is_codex_responses_endpoint; }
