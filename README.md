# cc-switch-lite

独立 Rust 单端口代理，为 Codex CLI 提供 `POST /v1/responses`，按模型选择 Provider。
支持 Responses 透传、Chat Completions 和 Anthropic Messages 转换（JSON/SSE、工具调用及思考内容）。

## 使用

需要 Rust stable、C 编译器及可访问 crates.io 的构建环境。

```sh
cargo build --release --locked
# 创建配置（不要把真实凭据提交到 Git）
install -m 600 config.example.yaml config.yaml
# 编辑地址和凭据后：
./target/release/cc-switch-lite validate --config config.yaml
./target/release/cc-switch-lite serve --config config.yaml
```

配置文件仅支持 YAML。直接编辑 [config.example.yaml](config.example.yaml) 的副本，字段、协议地址、
路由匹配及兼容选项见 [配置指南](docs/configuration.md)。单 Provider 可以只配置 `default_route`；
多个 Provider 可通过有序的 `routes` 列表按模型选择，首个匹配生效，路由不改写模型名。

每次修改后先运行 `validate`，成功时输出 `Configuration valid`，再重启服务。
服务使用启动时的配置快照；`serve` 和 `validate` 只读取文件，不会自动保存或重载配置。
配置包含明文凭据，Unix 下请保持 `chmod 600 config.yaml`。YAML 解析错误不会打印可能包含密钥的源码行。
`validate` 检查配置结构与约束，不会连接上游验证密钥、模型或服务可用性。

代理不提供入口鉴权，仅部署于可信内网或由外部网络层限制访问。默认示例监听 `0.0.0.0:15721`。
客户端自行配置 Responses endpoint 指向代理的 `/v1`，本程序不写入客户端配置。
仅向所选 Provider 注入该 Provider 凭据，不转发客户端认证头，禁用 HTTP 重定向与故障转移。
上游非成功状态保留上游 HTTP 状态；重定向不自动跟随，也不向客户端复制 `Location`。
JSON 错误脱敏后转换为 Responses 风格错误，保留上游错误类型和代码；非 JSON 诊断只进入脱敏后的错误消息。
上游响应中的已配置 API Key 会脱敏；普通原生 Responses 正文按原版条件透传，协议修复分支可能改写响应。
请求体上限为 200 MiB；缓冲响应正文上限为 128 MiB，SSE 按流处理。故障转移关闭时沿用原版行为，不启用
额外连接或响应空闲超时；流式请求保留 600 秒响应头等待上限和 24 小时总时限，
加密任务流识别仍有独立的有限预读窗口。

## 会话边界

Responses 的 `previous_response_id` 原样交上游。Chat 路径复用 `CodexChatHistoryStore`，
在进程内缓存最近 512 条含工具调用的响应，恢复工具结果之前的调用及 reasoning 内容；重启后缓存消失。
Messages 保持参考转换行为，不额外套用 Chat 缓存。
不提供跨 Provider/实例会话迁移、自定义模型映射、负载均衡、OAuth、管理 API 或热加载。

Chat/Messages 的截断和内容过滤通过 `response.incomplete` 报告；未完成的工具调用不提交执行。
两条路径支持 namespace 内的 custom 工具，并将 Codex `text.format` 的 JSON schema 映射到对应上游字段。
Messages 不支持无 schema 的 `json_object` 请求，转换时返回错误；实际 schema 和模型支持由上游校验。

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

协议兼容范围见 [请求兼容清单](docs/request-compatibility.md) 与 [行为清单](docs/compatibility.md)；
来源、版本和许可见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 维护入口

请求处理顺序位于 `src/compat/request.rs`，其私有子模块分别管理 Provider 配置投影和出站 headers。
`src/server.rs` 保留请求解析、单 Provider 重试和最终脱敏；私有 transport 模块负责一次上游发送及响应提交前校验。
`src/compat/response.rs` 负责成功响应转换，私有 error 模块集中客户端错误响应；现有模块导出路径保持不变。
重试使用首次准备的具名响应选项和原始工具上下文，不能因修正请求体而重新计算这些状态。
