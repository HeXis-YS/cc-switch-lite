# 固定基线与兼容行为清单

参考版本：`cc-switch@214e75ba7f625c679dd4e61df166ec4463245fe7`。
范围为 Codex `POST /v1/responses`、API Key、单 Provider、关闭故障转移。
下表是迁移审查契约；具体实现及测试必须保持所列触发条件，不能把未触发的其他应用路径当作缺陷补入。

## 请求链

| 上游来源 | 触发条件与顺序 | 轻量版接入 / 必要适配 |
| --- | --- | --- |
| `forwarder.rs` / `normalize_thinking_type` | 原始请求进入协议转换前 | 配置快照代替数据库；同样规范 thinking 类型 |
| `codex.rs` reasoning resolver | Chat；显式 meta、URL 推断、modelCatalog effort 等级 | `compat.request`，保留原解析函数 |
| `forwarder.rs` 1820–1870 | Chat：恢复历史 → 转换 → Gemini 签名补齐 → cache key | `compat.request`；进程内历史与客户端 session，不跨实例迁移 |
| `forwarder.rs` 1880–1925 | Messages：Provider 输出上限 → 转换/8192 缺省 → `[1m]` → 身份提示 → 缓存注入 | `compat.request`；cache injection 由子开关决定，此路径不受 optimizer 总开关限制 |
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

## 独立参考包与验证边界

`reference/cc-switch-reference` 不依赖生产 crate。源码通过 `git show <固定提交>:<路径>` 导出；`provenance.json` 记录每份来源和 SHA-256。转换模块按字节复制，Provider reasoning/optimizer 类型仅提取原声明；错误类型用只包含实际转换所需变体的基础设施替身。

`request_oracle.rs` 是根据固定 forwarder 顺序独立组装的请求体测试适配器，不读取生产实现。`header_oracle.rs` 则复用固定上游 header 循环、URL builder 和 session helpers，并只用 fixture 参数替代原依赖对象；差分测试比较 URL、完整 header map 及请求体，覆盖完整 endpoint、认证替换、协议 header、会话身份、`[1m]`、身份模拟、自定义 UA 和 OpenCode 路径。随机生成的 session UUID 仅在两侧都为 UUID 时规范化，其余值精确比较。

`tests/differential.rs` 还对比 JSON 请求/响应、错误、分片 SSE 事件及加密 agent task 的流提交行为。固定 ID 的流按字节比较，不屏蔽字段或事件顺序差异。独立 oracle 并非完整 server 替身；mock upstream 集成测试仍负责验证实际 socket 转发、压缩响应、错误路径和配置读取。真实 Codex CLI/远程 Provider 联调不包含在本地测试结论中。

重建参考包（要求 Python 3、Git 及含固定提交的 cc-switch 仓库）：

```sh
python3 scripts/reference-vendor.py ../cc-switch
```

运行差分检查（复用仓库锁文件，输出独立运行目录与版本/源码记录）：

```sh
bash scripts/reference-check.sh --locked
```

完整验收继续使用 `bash scripts/verify.sh`。不要对 verbatim 参考文件做自动重构/格式化；升级参考版本必须更新 pin、来源清单和差分证据。

## 本地入口差异

轻量版的无效 JSON 请求返回 400（原版 handler 将解析失败作为内部错误）；错误诊断 endpoint 固定标记公开入口 `/v1/responses`，不包含查询字符串，原版使用内部 `/responses` endpoint。凭据脱敏、配置快照和本地部署入口属于必要适配，并非原版服务器逐字复制。
