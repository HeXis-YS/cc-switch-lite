//! ProxyError 的用户可见错误消息。

use crate::proxy::error::ProxyError;

/// 将 ProxyError 转换为用户友好的错误消息
pub fn get_error_message(error: &ProxyError) -> String {
    match error {
        ProxyError::UpstreamError { status, body } => {
            // 上游没给内容时要明说，否则用户会以为是 CC Switch 把错误内容吞了（#6895）
            let body = body
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("上游未返回错误内容");
            format!("上游错误 ({status}): {body}")
        }
        ProxyError::Timeout(msg) => format!("请求超时: {msg}"),
        ProxyError::ForwardFailed(msg) => format!("转发失败: {msg}"),
        ProxyError::TransformError(msg) => format!("请求/响应转换错误: {msg}"),
        _ => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_error_message() {
        let error = ProxyError::UpstreamError {
            status: 500,
            body: Some("Internal Server Error".to_string()),
        };
        let msg = get_error_message(&error);
        assert!(msg.contains("上游错误"));
        assert!(msg.contains("500"));
        assert!(msg.contains("Internal Server Error"));
    }

    #[test]
    fn test_get_error_message_empty_body() {
        let error_empty = ProxyError::UpstreamError {
            status: 502,
            body: Some("   ".to_string()),
        };
        assert_eq!(
            get_error_message(&error_empty),
            "上游错误 (502): 上游未返回错误内容"
        );

        let error_none = ProxyError::UpstreamError {
            status: 502,
            body: None,
        };
        assert_eq!(
            get_error_message(&error_none),
            "上游错误 (502): 上游未返回错误内容"
        );
    }
}
