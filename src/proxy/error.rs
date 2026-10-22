use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Protocol conversion failed: {0}")]
    TransformError(String),
}
