//! Infrastructure stand-in: converters only construct these two error variants.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("无效的请求: {0}")]
    InvalidRequest(String),
    #[error("格式转换错误: {0}")]
    TransformError(String),
}
