// Derived from cc-switch 214e75ba, MIT; see providers/LICENSE-cc-switch.
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("上游响应体超过大小上限: {0} 字节")]
    ResponseBodyTooLarge(usize),

    #[error("请求转发失败: {0}")]
    ForwardFailed(String),

    #[error("上游错误 (状态码 {status}): {body:?}")]
    UpstreamError { status: u16, body: Option<String> },

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[allow(dead_code)]
    #[error("格式转换错误: {0}")]
    TransformError(String),

    #[allow(dead_code)]
    #[error("无效的请求: {0}")]
    InvalidRequest(String),

    #[error("超时: {0}")]
    Timeout(String),

    /// 认证错误
    #[error("认证失败: {0}")]
    AuthError(String),

    #[allow(dead_code)]
    #[error("内部错误: {0}")]
    Internal(String),
}

impl ProxyError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::ResponseBodyTooLarge(_) | Self::ForwardFailed(_) => StatusCode::BAD_GATEWAY,
            Self::UpstreamError { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::ConfigError(_) | Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::TransformError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::AuthError(_) => StatusCode::UNAUTHORIZED,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = match &self {
            ProxyError::UpstreamError {
                status: upstream_status,
                body: upstream_body,
            } => {
                // 尝试解析上游响应体为 JSON，如果失败则包装为字符串
                if let Some(body_str) = upstream_body {
                    if let Ok(json_body) = serde_json::from_str::<serde_json::Value>(body_str) {
                        // 上游返回的是 JSON，直接透传
                        json_body
                    } else {
                        // 上游返回的不是 JSON，包装为错误消息
                        json!({
                            "error": {
                                "message": body_str,
                                "type": "upstream_error",
                            }
                        })
                    }
                } else {
                    json!({
                        "error": {
                            "message": format!("Upstream error (status {})", upstream_status),
                            "type": "upstream_error",
                        }
                    })
                }
            }
            _ => {
                json!({
                    "error": {
                        "message": self.to_string(),
                        "type": "proxy_error",
                    }
                })
            }
        };

        (status, Json(body)).into_response()
    }
}

/// 错误分类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// 可重试错误（网络问题、5xx）
    Retryable, // 网络超时、5xx 错误
    /// 不可重试错误（4xx、认证失败）
    NonRetryable, // 认证失败、参数错误、4xx 错误
    #[allow(dead_code)]
    ClientAbort, // 客户端主动中断
}

/// 把错误连同 `source()` 原因链拼成一行。
///
/// reqwest / hyper 的外层文本只有「error sending request」「error decoding response body」
/// 这类描述，真正能区分 DNS、TCP、TLS、连接被重置的信息都在原因链里。
pub fn describe_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        // 有的错误会把原因也写进自己的文本，重复的就不再追加
        if !cause_text.is_empty() && !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// reqwest 错误的文本带完整 URL（查询参数里可能有 Key），先去掉 URL 再展开原因链。
pub fn describe_reqwest_error(error: reqwest::Error) -> String {
    describe_error_chain(&error.without_url())
}

/// 判断错误是否可重试
#[allow(dead_code)]
pub fn categorize_error(error: &reqwest::Error) -> ErrorCategory {
    if error.is_timeout() || error.is_connect() {
        return ErrorCategory::Retryable;
    }

    if let Some(status) = error.status() {
        if status.is_server_error() {
            ErrorCategory::Retryable
        } else if status.is_client_error() {
            ErrorCategory::NonRetryable
        } else {
            ErrorCategory::Retryable
        }
    } else {
        ErrorCategory::Retryable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_maps_each_proxy_error_variant() {
        let teapot = StatusCode::from_u16(418).unwrap();
        let cases = [
            (
                ProxyError::ResponseBodyTooLarge(128),
                StatusCode::BAD_GATEWAY,
            ),
            (
                ProxyError::ForwardFailed("connect failed".into()),
                StatusCode::BAD_GATEWAY,
            ),
            (
                ProxyError::UpstreamError {
                    status: 418,
                    body: None,
                },
                teapot,
            ),
            (
                ProxyError::ConfigError("bad config".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                ProxyError::TransformError("bad transform".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ProxyError::InvalidRequest("bad request".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                ProxyError::Timeout("slow upstream".into()),
                StatusCode::GATEWAY_TIMEOUT,
            ),
            (
                ProxyError::AuthError("bad credential".into()),
                StatusCode::UNAUTHORIZED,
            ),
            (
                ProxyError::Internal("unexpected".into()),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.status(), expected, "{error:?}");
        }
    }

    #[derive(Debug)]
    struct Outer(std::io::Error);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("error sending request")
        }
    }

    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn describe_error_chain_appends_every_cause() {
        let error = Outer(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "tcp connect error",
        ));
        assert_eq!(
            describe_error_chain(&error),
            "error sending request: tcp connect error"
        );
    }

    #[tokio::test]
    async fn describe_reqwest_error_keeps_cause_and_drops_url() {
        // 先占一个端口再释放，保证连接被拒绝
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let error = reqwest::Client::new()
            .get(format!(
                "http://127.0.0.1:{port}/v1/responses?key=secret-token"
            ))
            .send()
            .await
            .expect_err("connection should be refused");
        assert!(error.is_connect());

        let text = describe_reqwest_error(error);
        assert!(text.starts_with("error sending request: "), "{text}");
        assert!(!text.contains("secret-token"), "{text}");
    }
}
