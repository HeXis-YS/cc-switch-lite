# 固定基线与兼容行为清单

当前转发逻辑来自 **cc-switch v4.0.18**。
迁移来源提交：`214e75ba7f625c679dd4e61df166ec4463245fe7`，包含用户的本地提交。
范围为 Codex `POST /v1/responses`、API Key、单 Provider、关闭故障转移。
下表是迁移审查契约；具体实现及测试必须保持所列触发条件，不能把未触发的其他应用路径当作缺陷补入。

## 请求链

| 上游来源 | 触发条件与顺序 | 轻量版接入 / 必要适配 |
| --- | --- | --- |
| `forwarder.rs` / `normalize_thinking_type` | 原始请求进入协议转换前 | 配置快照代替数据库；同样规范 thinking 类型 |
| `codex.rs` reasoning resolver | Chat；显式 meta、URL 推断、modelCatalog effort 等级 | `compat.request`，保留原解析函数 |
| `forwarder.rs` 1820–1870 | Chat：恢复历史 → 转换 → Gemini 签名补齐 → cache key | `compat.request`；进程内历史与客户端 session，不跨实例迁移 |
| `forwarder.rs` 1880–1925 | Messages：Provider 输出上限 → 转换/128000 缺省 → `[1m]` → 身份提示 → 缓存注入 | `compat.request`；缺省输出预算由上游的 8192 调整为 128000，包含思考消耗；cache injection 由子开关决定，此路径不受 optimizer 总开关限制 |
| `codex_compaction.rs` | 第三方原生 Responses 压缩摘要/触发转换 | `compat.request`；官方 OAuth 状态清洗不适用 |
| namespace / xAI sanitize | 原生 Responses 且原版 Provider 判定命中 | `compat.request`；须保留识别条件、schema 和工具名恢复 |
| Moonshot schema | Chat 且 Moonshot/Kimi URL 命中 | `$ref` 同级关键词移入 `allOf`；其他上游不改 |
| media sanitizer | enabled + requestMediaFallback；显式 text-only 或允许 heuristic 的已知名单 | 原始媒体语义保留；不得把启发式关闭解释为关闭反应式重试 |
| body filter / canonical JSON | 转换和媒体处理后 | 私有 `_` 字段过滤、排序；override 后再执行一次 |
| localProxyRequestOverrides | 非 Copilot API Key 请求 | 保留 body 深度合并、stream 保护及敏感 header 保护 |
| URL / header helpers | 选定协议、完整 endpoint、客户端 header | 保留 URL 防重复、认证替换、协议 beta/version、指纹过滤及 identity 编码请求 |

## 响应及重试链

| 上游来源 | 触发条件与行为 | 验证重点 |
| --- | --- | --- |
| content_encoding | 压缩成功体、错误体 | gzip/deflate/br/zstd、未知编码、解压后限额；透明响应避免二次编码 |
| handler buffered conversion | Chat/Messages 非流式 | JSON 失败后 SSE 嗅探；工具/思考/usage/错误均保留 |
| streaming converters | Chat/Messages 流 | UTF-8 跨分片、混合换行、尾部无空行、工具参数、签名、流错误 |
| Messages success validator | HTTP 2xx 的非流式或显式 JSON 响应 | 错误信封在响应提交前识别 |
| error mapper / context window | 失败响应和符合原版条件的上下文超限 | 状态码、JSON 与 `response.failed` SSE，错误诊断脱敏 |
| native compaction / namespace / late arguments | 原生 Responses | 转换开关/顺序、普通无关输出不变 |
| media retry | Codex + enabled + requestMediaFallback + 含图片 + 不支持图片错误 | 替换图片，对同一 Provider 重试一次；失败终止该分支，不轮询其他 Provider |
| opaque retry | 原生 Responses + enabled + 明确密文拒绝；普通兼容错误另受第三方门控 | 至多一次；不破坏加密 agent 任务；剩余不可解任务返回原版专用错误 |
| encrypted task stream hold | 原生 Responses + enabled + 请求尾部加密 agent 任务 | 输出前识别失败；单一 10 秒截止和 256 KiB 上限；非拒绝字节原样重放 |
| signature/budget retries | **仅 ProviderType Claude/ClaudeAuth** | Codex 转 Messages 仍是 Codex Provider，因此不能因输出协议就启用这些重试 |
| timeouts | handler context 在 failover=false 时传零 | 保留 forwarder 的流式 600 秒响应头等待和 24 小时总时限；不额外添加连接或 idle 超时；加密任务自身 hold 上限仍有效 |

故障转移、熔断计量、stack 模型映射、官方账号/OAuth、Copilot、Bedrock 专用分支及其他客户端入口明确范围外。原版日志中的上游原始内容不能照搬到生产日志：凭据脱敏是已约定差异。

## 本地回归验证

迁移已在本项目提交 `58c8c87` 验收。来源是包含用户本地提交的 cc-switch 仓库，不等同于公共上游版本。后续重构以本项目重构前行为为基线，不持续与 cc-switch 比较。

`tests/regression.rs` 保留迁移验收使用的输入，期望值固定于 `tests/fixtures/regression`。JSON 比较完整结构；固定 ID 的 SSE 在 1、7、4096 字节分片下按字节比较；URL 与完整 header map 保持断言。仅原场景中自动生成的 session UUID 规范化为固定 UUID，其他字段和事件顺序不忽略。加密任务拒绝与提交测试保留直接断言。

参考源码副本及生成工具已移除。测试不生成或更新期望值；行为重构不能通过修改期望值来掩盖变化。原版单元测试与 mock HTTP 集成测试继续保护转换、历史、重试、压缩、错误和配置行为。真实 Codex CLI/远程 Provider 联调不包含在这些本地测试结论中。

运行 `bash scripts/verify.sh` 完整验证，无需 cc-switch 仓库或真实 Provider 账户。

## 本地入口差异

轻量版的无效 JSON 请求返回 400（原版 handler 将解析失败作为内部错误）；错误诊断 endpoint 固定标记公开入口 `/v1/responses`，不包含查询字符串，原版使用内部 `/responses` endpoint。凭据脱敏、配置快照和本地部署入口属于必要适配，并非原版服务器逐字复制。
