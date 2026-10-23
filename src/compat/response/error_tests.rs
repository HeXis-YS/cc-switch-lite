use super::*;
#[test]
fn codex_proxy_forward_error_includes_context_and_cause() {
    let error = ProxyError::ForwardFailed("连接失败: dns lookup failed".to_string());
    let body = codex_proxy_error_json("DeepSeek", "deepseek-chat", "/responses", &error);

    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("CC Switch local proxy failed"));
    assert!(message.contains("DeepSeek"));
    assert!(message.contains("deepseek-chat"));
    assert!(message.contains("/responses"));
    assert!(message.contains("dns lookup failed"));
    assert_eq!(body["error"]["code"], "cc_switch_forward_failed");
    assert_eq!(body["error"]["provider"], "DeepSeek");
    assert_eq!(body["error"]["model"], "deepseek-chat");
}

#[test]
fn codex_proxy_upstream_error_normalizes_nonstandard_body() {
    let error = ProxyError::UpstreamError {
        status: 502,
        body: Some(
            r#"{"base_resp":{"status_code":2013,"status_msg":"upstream gateway failed"}}"#
                .to_string(),
        ),
    };
    let body = codex_proxy_error_json("MiniMax", "abab6.5s", "/responses", &error);

    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains("upstream_status: HTTP 502"));
    assert!(message.contains("upstream gateway failed"));
    assert_eq!(body["error"]["code"], 2013);
    assert_eq!(body["error"]["upstream_status"], 502);
}

#[test]
fn codex_proxy_413_points_to_upstream_not_local_proxy() {
    // 模拟上游渠道商 nginx 因 client_max_body_size 返回的 413 HTML 页面
    // （见 issue #666：长上下文 / 大图 / 大日志撞上游体积上限）
    let error = ProxyError::UpstreamError {
        status: 413,
        body: Some(
            "<html>\r\n<head><title>413 Request Entity Too Large</title></head>\r\n\
                 <body>\r\n<center><h1>413 Request Entity Too Large</h1></center>\r\n\
                 <hr><center>nginx/1.29.6</center>\r\n</body>\r\n</html>"
                .to_string(),
        ),
    };
    let body = codex_proxy_error_json("HCAI", "gpt-5.5", "/responses", &error);

    let message = body["error"]["message"].as_str().unwrap();
    // 不再误导成「本地代理失败」
    assert!(!message.contains("CC Switch local proxy failed"));
    // 明确指向上游 + 体积超限 + 可操作指引
    assert!(message.contains("413"));
    assert!(message.to_lowercase().contains("upstream"));
    assert!(message.contains("/compact"));
    // 关键：不把整段 nginx HTML 回显给用户
    assert!(!message.contains("<html>"));
    assert!(!message.contains("nginx/1.29.6"));
    // 结构化字段仍然保留，便于程序化消费 / UI 呈现
    assert_eq!(body["error"]["upstream_status"], 413);
    assert_eq!(body["error"]["provider"], "HCAI");
    assert_eq!(body["error"]["model"], "gpt-5.5");
    assert_eq!(body["error"]["endpoint"], "/responses");
}
