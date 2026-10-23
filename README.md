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

配置结构见 [config.example.toml](config.example.toml)，可选兼容字段和固定参考版本的差异清单见
[请求兼容清单](docs/request-compatibility.md) 与 [行为清单](docs/compatibility.md)。`base_url` 是 API 前缀；
程序按所选协议构造 `/responses`、`/chat/completions` 或 `/v1/messages`，并处理已带版本号的前缀。
不实现聚合模型路由或用户自定义模型映射；协议兼容分支仍可能按原版语义处理模型标记。
配置版本必须为 1；Provider ID 唯一；当前路由必须存在；未知字段和无效凭据拒绝启动。
TOML 错误不会打印可能包含密钥的源码行。程序保存配置时在同目录创建临时文件、同步后原子替换，Unix 权限为 `0600`。

代理不提供入口鉴权，仅部署于可信内网或由外部网络层限制访问。默认示例监听 `0.0.0.0:15721`。
客户端自行配置 Responses endpoint 指向代理的 `/v1`，本程序不写入客户端配置。
仅向所选 Provider 注入该 Provider 凭据，不转发客户端认证头，禁用 HTTP 重定向与故障转移。
上游非成功状态保留上游 HTTP 状态；重定向不自动跟随，也不向客户端复制 `Location`。
JSON 错误脱敏后转换为 Responses 风格错误，保留上游错误类型和代码；非 JSON 诊断只进入脱敏后的错误消息。
上游响应中的已配置 API Key 会脱敏；普通原生 Responses 正文按原版条件透传，协议修复分支可能改写响应。
请求体上限为 200 MiB；缓冲响应正文上限为 128 MiB，SSE 按流处理。故障转移关闭时沿用原版行为，不启用
额外连接或响应空闲超时；流式请求保留 600 秒响应头等待上限和 24 小时总时限，
加密任务流识别仍有独立的有限预读窗口。

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
测试包含保留的原版单元测试、以本项目 `58c8c87` 为基线的固定回归场景、配置原子替换/权限和本地 mock HTTP/SSE 集成测试。
迁移已验收，后续重构不依赖 cc-switch 副本。
不需要真实 Provider 账户。Mock 通过不代表已验证真实 Codex CLI 与所有远程供应商组合。

Unix / Python 3.11+ 可在构建后运行 `python3 scripts/tui-smoke.py`，通过真实 PTY
检查新增 Provider、密钥掩码、0600 权限和终端恢复；日志位于 `.codex/runs/tui-<UTC>-<PID>/`。

协议及 TUI 来源、版本和许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
