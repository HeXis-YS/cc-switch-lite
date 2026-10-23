# cc-switch-lite

独立 Rust 单端口代理与 TUI 控制面板，为 Codex CLI 提供 `POST /v1/responses`，按模型选择 Provider。
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
`[[routes]]` 按顺序匹配完整的请求 `model`，首个匹配生效。`model_pattern` 支持 glob：
`claude-*`、`gpt-[45]*`、`deepseek/*`；`*` 不跨 `/`，跨路径匹配使用 `**`。
未命中时使用 `[default_route]`；未配置默认路由则返回请求错误。请求必须包含非空字符串 `model`。
路由只选择 Provider，不改写模型名：`claude-*` 不匹配 `ccs-openlux-claude/claude-opus-5-5`，
需由客户端发送上游接受的模型名，或配置匹配完整名称的规则。协议兼容分支仍可能按原版语义处理模型标记。
旧 `[route] provider = "..."` 仍可读取，作为未设置 `default_route` 时的后备；TUI 设置默认路由后改用新格式。
配置版本必须为 1；Provider ID、规则名称唯一；路由引用必须存在；未知字段和无效凭据拒绝启动。
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

`Tab` 切换 Provider 和路由列表。列表中 `↑/↓` 选择，`a` 新增、`e` 编辑、`d` 删除、`v` 校验、`q` 退出。
Provider 列表中 `Enter` 设置默认路由；路由列表中 `K/J` 上移/下移规则，`x` 清除默认路由（必须保留至少一条规则）。
路由编辑器填写名称、模型 glob 和已有 Provider ID；规则按显示顺序匹配。
编辑时 `Tab/Shift-Tab` 切字段，协议字段用左右键选择，`Ctrl-S` 保存，`Esc` 取消。
API Key 始终掩码显示。被规则或默认路由引用的 Provider 不能删除；修改 Provider ID 会同步更新引用。
不存在的配置可以通过 TUI 新增首个 Provider 创建。
每次成功修改立即保存并提示重启；运行中的服务继续使用启动快照。

## 会话边界

Responses 的 `previous_response_id` 原样交上游。Chat 路径复用 `CodexChatHistoryStore`，
在进程内缓存最近 512 条含工具调用的响应，恢复工具结果之前的调用及 reasoning 内容；重启后缓存消失。
Messages 保持参考转换行为，不额外套用 Chat 缓存。
不提供跨 Provider/实例会话迁移、自定义模型映射、负载均衡、OAuth、管理 API 或热加载。

Messages 转换对 Opus 5.5 应用 [cc-switch #8048](https://github.com/farion1231/cc-switch/pull/8048)，
并按本项目指定范围对 Sonnet 5.5、Fable 5.1、Haiku 5.5 启用相同行为：保留 adaptive thinking，允许完整的无签名工具历史。
强制工具选择转为 `auto` 并追加调用指令；指定工具时仅保留该工具。这是尽力满足调用意图，不能保证强制调用。
**Haiku 5.5 未经验证**，该兼容行为是否适用于真实上游尚未确认；本地回归测试不代表这些模型已通过远程供应商验证。

## 验证

```sh
./scripts/verify.sh
```

入口固定仓库目录，运行 `cargo fmt --check`、`cargo test --locked` 和
`cargo clippy --locked --all-targets -- -D warnings`。唯一运行日志、代码差异及工具版本记录于 `.codex/runs/<UTC>-<PID>/`。
默认构建和测试并发数均为 2，可通过 `CARGO_BUILD_JOBS`、`RUST_TEST_THREADS` 覆盖，实际值写入日志。
测试包含保留的原版单元测试、以本项目 `58c8c87` 为基线的固定回归场景、配置原子替换/权限和本地 mock HTTP/SSE 集成测试。
迁移已验收，后续重构不依赖 cc-switch 副本。
不需要真实 Provider 账户。Mock 通过不代表已验证真实 Codex CLI 与所有远程供应商组合。

Unix / Python 3.11+ 可在构建后运行 `python3 scripts/tui-smoke.py`，通过真实 PTY
检查新增 Provider、路由增删改与排序、清除默认路由、密钥掩码、0600 权限和终端恢复；日志位于 `.codex/runs/tui-<UTC>-<PID>/`。

协议及 TUI 来源、版本和许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 维护入口

请求处理顺序位于 `src/compat/request.rs`，其私有子模块分别管理 Provider 配置投影和出站 headers。
`src/server.rs` 保留请求解析、单 Provider 重试和最终脱敏；私有 transport 模块负责一次上游发送及响应提交前校验。
`src/compat/response.rs` 负责成功响应转换，私有 error 模块集中客户端错误响应；现有模块导出路径保持不变。
重试使用首次准备的具名响应选项和原始工具上下文，不能因修正请求体而重新计算这些状态。
