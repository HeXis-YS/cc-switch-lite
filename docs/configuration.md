# YAML 配置指南

配置由 `serve --config <path>` 或 `validate --config <path>` 显式指定，只支持 YAML。
可以从仓库根目录的 [config.example.yaml](../config.example.yaml) 复制一份，再修改地址和密钥。
两个命令都只读取文件；修改后先校验，再重启服务，运行中的进程继续使用启动快照。

```sh
install -m 600 config.example.yaml config.yaml
# 用文本编辑器修改 config.yaml 后：
./target/release/cc-switch-lite validate --config config.yaml
./target/release/cc-switch-lite serve --config config.yaml
```

`validate` 成功时输出 `Configuration valid`，失败时返回非零退出码。
它不发送上游请求，因此不会验证密钥是否有效、模型是否存在或服务是否可达。
API Key 直接填写字符串，不支持环境变量替换；例如 `"${API_KEY}"` 会作为字面值读取。
配置含明文凭据，请勿提交真实密钥，Unix 下保持文件权限为 `0600`。
模型 glob、地址和密钥建议用引号包裹，尤其是 `"*"`、`"**"` 等 YAML 中有特殊含义的字符串。

## 单 Provider 最小配置

只需一个 Provider 和一条默认路由，所有请求模型都会交给该 Provider：

```yaml
version: 1
server:
  listen: "127.0.0.1:15721"
default_route:
  provider: main
providers:
  - id: main
    name: Main
    protocol: responses
    base_url: "https://example.com/v1"
    api_key: "replace-with-your-key"
```

客户端仍需自行将 Responses endpoint 指向代理的 `/v1`，并发送上游接受的模型名。
代理不提供入口鉴权；监听非回环地址时，应部署在可信内网或由外部网络层限制访问。

## 顶层字段

| 字段 | 必需 / 默认值 | 含义 |
| --- | --- | --- |
| `version` | 必需，必须为 `1` | 配置版本 |
| `server.listen` | 必需，无文件默认值 | 监听 IP 和端口，例如 `"127.0.0.1:15721"`；不是 URL 或主机名 |
| `providers` | 必需 | Provider 列表，字段见下表 |
| `default_route.provider` | 可省略 | 未命中任何规则时使用的 Provider ID |
| `routes` | 默认 `[]` | 按顺序匹配完整请求模型名的规则列表 |
| `compat` | 可省略 | 全局兼容选项，通常沿用默认值即可 |
| `route.provider` | 可省略 | 旧的默认 Provider 字段；仅在没有 `default_route` 时生效，新配置使用 `default_route` |

必须至少配置一条 `routes` 规则或一个默认 Provider。所有路由引用必须指向已有 Provider；
即使旧 `route.provider` 被 `default_route` 覆盖，它也必须引用存在的 Provider。
示例中的 `0.0.0.0:15721` 是显式配置值，省略 `server.listen` 会校验失败。

## Provider 字段与协议

| `providers[]` 字段 | 约束与用途 |
| --- | --- |
| `id` | 必需、非空且唯一；供路由引用 |
| `name` | 必需、非空；用于供应商识别，也可能参与上游兼容逻辑的自动推断 |
| `protocol` | 必需：`responses`、`chat_completions`、`messages` 之一 |
| `base_url` | 必需：HTTP(S) 地址，不能含用户名、密码或 fragment；默认也不允许 query |
| `api_key` | 必需、非空且能用作 HTTP header 值；代理为选中的 Provider 注入该凭据 |
| `compat` | 可省略：该 Provider 的兼容选项，见后文 |

通常填写 API 前缀，由代理拼接协议路径：

| `protocol` | `base_url` 示例 | 上游地址 |
| --- | --- | --- |
| `responses` | `https://example.com/v1` | `https://example.com/v1/responses` |
| `chat_completions` | `https://example.com/v1` | `https://example.com/v1/chat/completions` |
| `messages` | `https://example.com/v1` | `https://example.com/v1/messages` |

`responses` 使用原生 Responses 路径；另外两种协议转换请求、JSON 响应和 SSE。
原版兼容分支可能修复或转换 Responses 内容，具体见 [行为清单](compatibility.md)。
Provider 失败直接返回错误，不会改用其他 Provider；符合条件的请求修正只会在同一 Provider 上重试。

如果供应商给的是完整 endpoint，在该 Provider 下设置 `compat.meta.isFullUrl: true`，
此时 `base_url` 可含 query。Chat 和 Messages 对标准完整 endpoint 也有自动识别；
普通接入优先使用上表中的 API 前缀写法。

## 按模型分流

[完整示例](../config.example.yaml) 将 `claude-*` 发给 Messages Provider，将 `gpt-[45]*`
发给 Chat Provider，其他模型使用默认 Responses Provider。

每条 `routes[]` 规则包含三个必填字符串：

| 字段 | 含义 |
| --- | --- |
| `name` | 非空且唯一的规则名称 |
| `model_pattern` | 非空的 glob，匹配整个请求 `model`；不是 regex |
| `provider` | 目标 Provider 的 `id` |

规则按列表顺序检查，首个匹配生效，没有单独的优先级字段。
精确规则应放在宽泛规则前，例如 `gpt-4.1` 放在 `gpt-*` 前；`"**"` 放在末尾可以接住所有模型。
未命中规则时，依次检查 `default_route`、旧 `route.provider`；都没有就返回请求错误。
如果只配置 `routes`，没有默认 Provider，未匹配的模型将失败。

| glob 示例 | 匹配示例 | 不匹配示例 |
| --- | --- | --- |
| `gpt-4.1` | `gpt-4.1` | `gpt-4.1-mini` |
| `claude-*` | `claude-opus-5-5` | `vendor/claude-opus-5-5` |
| `gpt-[45]*` | `gpt-4o`、`gpt-5` | `gpt-3.5` |
| `deepseek/*` | `deepseek/deepseek-chat` | `deepseek/team/model` |
| `**` | `gpt-4.1`、`vendor/model` | — |

`*` 不跨 `/`，跨路径匹配使用 `**`。路由不添加、删除或映射模型前缀：
请求 `vendor/claude-opus-5-5` 时，规则也需要匹配完整名称，且上游必须接受该名称。
协议兼容分支对模型标记的处理另见 [请求兼容清单](request-compatibility.md)。

手工维护时：新增 Provider 后再添加引用它的规则；改名 `id` 时同步修改所有路由引用；
删除 Provider 前先删除或改指向相关规则和默认路由。每次修改后运行 `validate`，再重启服务。

## 可选兼容配置

顶层配置主要使用 snake_case，`meta` 内的字段沿用上游 camelCase；大小写和下划线不能互换。
下例是一份可独立使用的 Messages 配置，仅调整输出上限和图片预判策略：

```yaml
version: 1
server:
  listen: "127.0.0.1:15721"
compat:
  rectifier:
    requestMediaHeuristic: false
default_route:
  provider: messages
providers:
  - id: messages
    name: Messages
    protocol: messages
    base_url: "https://example.com/v1"
    api_key: "replace-with-your-key"
    compat:
      meta:
        maxOutputTokens: 16384
```

### Provider 兼容选项

以下字段位于 `providers[].compat.meta`，均可省略：

| 字段 | 默认行为 / 用途 |
| --- | --- |
| `codexChatReasoning` | Chat reasoning 能力声明；缺省时按 Provider 名称、地址与请求模型自动推断 |
| `promptCacheRouting` | `auto`（缺省）、`enabled`、`disabled`；控制 Chat prompt cache key 注入 |
| `maxOutputTokens` | 缺省或 `0` 不覆盖客户端值；正整数在 Messages 转换前覆盖输出上限 |
| `impersonateClaudeCode` | 默认 `false`；启用原版 Claude Code 身份相关兼容处理 |
| `isFullUrl` | 默认 `false`；将 `base_url` 作为完整上游 endpoint |
| `customUserAgent` | 非空值替换出站 User-Agent，须为有效 HTTP header 值 |
| `localProxyRequestOverrides` | `headers` 映射与可选的 `body` 对象；用于请求覆盖 |

`codexChatReasoning` 可设置 `supportsThinking`、`supportsEffort`、`thinkingParam`、`effortParam`
和 `effortValueMode`，用于覆盖自动推断；转换语义见 [请求兼容清单](request-compatibility.md#configuration-mapping)。
其 `outputFormat` 目前是声明字段，不控制响应解析。覆盖 `body.model` 会校验失败；
`body.stream` 沿用上游行为，不作为覆盖项生效。认证、目标地址和消息分帧相关 headers 不能覆盖。

`providers[].compat.settings_config` 默认 `{}`，只允许顶层 `modelCatalog`、`models` 两个键，
用于每模型 reasoning 档位和媒体能力声明，不能放第二份地址、密钥或模型映射。
它不会自动生成路由，仍需在 `routes` 中配置 Provider 选择。
Messages 未指定 Provider 上限且请求也未给上限时，转换默认使用 `128000`，实际支持值由上游决定。

### 全局兼容选项

以下路径位于顶层 `compat` 下：

| 路径 | 默认值 | 用途 |
| --- | --- | --- |
| `optimizer.enabled` | `false` | 保留原版优化器配置；Messages 的桥接缓存独立于该总开关 |
| `optimizer.thinkingOptimizer` | `true` | 保留原版字段；Messages 桥接路径固定关闭 thinking 优化 |
| `optimizer.cacheInjection` | `true` | 控制 Messages 的标准缓存标记注入 |
| `rectifier.enabled` | `true` | 请求修正总开关 |
| `rectifier.requestThinkingSignature` | `true` | thinking 签名相关修正 |
| `rectifier.requestThinkingBudget` | `true` | thinking budget 相关修正 |
| `rectifier.requestMediaFallback` | `true` | 图片降级总开关，包含发送前处理和上游拒绝后的修正 |
| `rectifier.requestMediaHeuristic` | `true` | 未显式声明能力时，启用内置纯文本模型注册表的发送前预判 |

关闭 `requestMediaHeuristic` 只停用注册表预判，不关闭显式纯文本声明的处理或上游拒绝后的修正。
`transport.streamingFirstByteTimeout`、`transport.streamingIdleTimeout`、`transport.nonStreamingTimeout`
默认均为 `0`，本项目不启用故障转移，因此非零值会校验失败，通常省略整个 `transport`。

## 校验边界与排查

`validate` 检查版本、必填字段、ID 与规则名称唯一性、Provider 引用、地址、凭据和受限兼容选项。
顶层配置、`providers[]`、`routes[]`、`default_route`、旧 `route`、顶层及 Provider 的 `compat`、
`compat.transport`、Provider 的 `compat.meta` 和 `localProxyRequestOverrides` 对象会拒绝未知字段。
`server`、`optimizer`、`rectifier` 和 `codexChatReasoning` 的未知字段沿用当前反序列化行为，可能被忽略；
不能依赖 `validate` 发现这些子结构中的所有拼写错误。

| 现象 | 检查方向 |
| --- | --- |
| `Invalid YAML configuration` | 检查缩进、引号、字段名称、类型和必填字段；为避免泄露密钥，解析错误不显示源码行 |
| 路由引用不存在的 Provider | 核对 `provider` 与 `providers[].id` 是否完全一致 |
| 校验成功但模型仍失败 | 校验不会访问上游；检查实际模型名、规则顺序、协议、地址与密钥 |
| `claude-*` 未匹配带前缀模型 | glob 匹配完整模型名，`*` 不跨 `/`，按实际名称调整规则 |
| 修改后请求仍使用旧配置 | 重启服务；运行进程不会重新读取文件 |
