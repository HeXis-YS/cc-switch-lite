# cc-switch-lite

独立 Rust 单 Provider 路由代理，为 Codex CLI 提供 `POST /v1/responses`。
支持 Responses 透传、Chat Completions 和 Anthropic Messages 转换（JSON/SSE、工具调用及思考内容）。

## 使用

需要 Rust stable、C 编译器及可访问 crates.io 的构建环境。

```sh
cargo build --release --locked
# 创建配置（不要把真实凭据提交到 Git）
install -m 600 config.example.toml config.toml
# 编辑地址和凭据后：
./target/release/cc-switch-lite validate --config config.toml
./target/release/cc-switch-lite serve --config config.toml
# 另一终端：
./target/release/cc-switch-lite tui --config config.toml
```

配置结构见 [config.example.toml](config.example.toml)。`base_url` 是 API 前缀（通常以 `/v1` 结尾），
程序分别追加 `/responses`、`/chat/completions` 或 `/messages`。保留请求中的 `model`，不映射模型。
配置版本必须为 1；Provider ID 唯一；当前路由必须存在；未知字段和无效凭据拒绝启动。
TOML 错误不会打印可能包含密钥的源码行。程序保存配置时在同目录创建临时文件、同步后原子替换，Unix 权限为 `0600`。

代理不提供入口鉴权，仅部署于可信内网或由外部网络层限制访问。默认示例监听 `0.0.0.0:15721`。
客户端自行配置 Responses endpoint 指向代理的 `/v1`，本程序不写入客户端配置。
仅向所选 Provider 注入该 Provider 凭据，不转发客户端认证头，禁用 HTTP 重定向与故障转移。
上游非成功状态保留 HTTP 状态（重定向变为 502），JSON 错误脱敏后转换为 Responses 风格错误；非 JSON 错误返回固定消息。
上游正文中的已配置 API Key 会脱敏；因此 Responses 在不含密钥时保持正文透传。
JSON 响应上限 32 MiB；请求使用 Axum 默认 2 MiB 限制；SSE 按流传输；连接超时 15 秒、上游读空闲超时 120 秒。

## TUI

列表中 `↑/↓` 选择，`a` 新增、`e` 编辑、`d` 删除、`Enter` 设为当前路由、`v` 校验、`q` 退出。
编辑时 `Tab/Shift-Tab` 切字段，协议字段用左右键选择，`Ctrl-S` 保存，`Esc` 取消。
API Key 始终掩码显示。当前 Provider 需先切换到另一项后才能删除。
不存在的配置可以通过 TUI 新增首个 Provider 创建。
每次成功修改立即保存并提示重启；运行中的服务继续使用启动快照。

## 会话边界

Responses 的 `previous_response_id` 原样交上游。Chat 路径复用 `CodexChatHistoryStore`，
在进程内缓存最近 512 条含工具调用的响应，恢复工具结果之前的调用及 reasoning 内容；重启后缓存消失。
Messages 保持参考转换行为，不额外套用 Chat 缓存。
不提供跨 Provider/实例会话迁移、模型聚合、规则、映射、OAuth、管理 API 或热加载。

## 验证

```sh
./scripts/verify.sh
```

入口固定仓库目录，运行 `cargo fmt --check`、`cargo test --locked` 和
`cargo clippy --locked --all-targets -- -D warnings`。唯一运行日志、代码差异及工具版本记录于 `.codex/runs/<UTC>-<PID>/`。
测试包含抽取的上游单元测试、配置原子替换/权限和本地 mock HTTP/SSE 集成测试。
不需要真实 Provider 账户。Mock 通过不代表已验证真实 Codex CLI 与所有远程供应商组合。

Unix / Python 3.11+ 可在构建后运行 `python3 scripts/tui-smoke.py`，通过真实 PTY
检查新增 Provider、密钥掩码、0600 权限和终端恢复；日志位于 `.codex/runs/tui-<UTC>-<PID>/`。

协议及 TUI 来源、版本和许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
