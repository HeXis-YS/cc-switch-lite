# Request compatibility inventory

Baseline: `farion1231/cc-switch@214e75ba7f625c679dd4e61df166ec4463245fe7`.
This inventory covers `AppType::Codex`, an explicit API-key Provider, route mode,
and `POST /v1/responses`. It does not claim equivalence to OAuth, Stack, or the
other inbound protocols. Copied modules retain their original tests and MIT
copyright; extraction helpers retain original function bodies unless listed below.

| Original execution point | Local execution | Disposition |
| --- | --- | --- |
| `session::extract_session_id` | Copied `proxy/session.rs`, called before conversion | Full extraction precedence, metadata fallback, UUID and client-provided distinction retained |
| `forwarder::normalize_thinking_type` | `upstream_request/forwarder.rs` before request conversion | Current upstream identity function retained with its tests |
| Conversation fingerprint and OpenCode session selection | Copied helpers, fingerprint before conversion; header injection after URL resolution | Native header, extracted session, conversation fingerprint, then generated UUID precedence retained |
| `[1m]` stripping before conversion | Non-Anthropic paths strip using original helper | Anthropic defers stripping until its converted request so beta detection still works |
| Chat history enrichment | `CodexChatHistoryStore::enrich_request` | Runs before reasoning resolution and conversion |
| Chat reasoning config | Extracted `codex.rs` resolver and inference closure | Explicit metadata, normalization, platform precedence, model inference and Zen catalog effort lookup retained |
| Gemini tool thought signature fallback | Production invocation after Chat conversion | Existing function re-enabled outside tests; only first call in current-turn assistant steps is filled |
| Chat cache routing | Extracted original cache eligibility and injection | Explicit nonempty client key wins, then real client session; unknown gateways default to no injection |
| Anthropic output ceiling | Metadata override before conversion | Positive `maxOutputTokens` beats client value; absent both, conversion defaults to 128000 tokens (including thinking), intentionally differing from upstream's 8192 fallback |
| Anthropic identity and cache | Original identity helper and complete cache injector | Impersonation opt-in; bridge cache enabled independent of optimizer master switch; `cacheInjection` respected |
| Native Responses compaction | Complete copied module | Known summary replay and compaction trigger rewriting; unknown ciphertext preserved until reactive recovery |
| Native xAI | Complete namespace and xAI modules | Restore map captured before flattening, then xAI field/schema/orphan-output rewrites; configured upstream model absent by scope |
| Moonshot/Kimi schema | Complete copied module | Runs only on Chat after signature/cache handling and only for original upstream URL gate |
| Media prevention | Complete media sanitizer using projected Provider | After conversion and Moonshot rewrite, before private filtering; original master/fallback/heuristic gates |
| Private request fields and stable serialization | Complete body filter plus existing canonicalizer re-enabled in production | Recursive private-field rules preserve schema property names; JSON canonicalized before overrides and again if changed |
| Local body overrides | Original deep merge | Protected top-level `stream` ignored as upstream; user model override rejected at config boundary as outside scope |
| URL construction | Original Codex adapter URL builder and full endpoint/query helpers | Origin `/v1` insertion, custom prefixes, duplicate version cleanup, full Chat/Messages endpoints, query concatenation retained |
| Request headers | Original header loop, scope-dependent booleans supplied by adapter | Auth replacement, fingerprint filtering, identity encoding, Anthropic accept/beta/version, custom UA, OpenCode fallback and protected override handling retained |
| Managed credential placeholder guard | Original guard and helpers | Prevents PROXY_MANAGED leakage even if API-key configuration points to a managed-account URL |

## Configuration mapping

All metadata names retain the original camelCase serialization:

| Local `providers[].compat.meta` | Default and use |
| --- | --- |
| `codexChatReasoning` | Absent: infer from provider name, URL and requested model; original inner fields and priority retained |
| `promptCacheRouting` | Absent / `auto`: only known eligible Chat hosts; `enabled` and `disabled` override |
| `maxOutputTokens` | Absent / zero: no Provider override; positive value injected before Anthropic transform |
| `impersonateClaudeCode` | Absent / false: no identity/UA/x-app/Claude Code beta injection |
| `isFullUrl` | Absent / false: normal Codex URL builder, with original Chat/Messages complete-endpoint guards |
| `customUserAgent` | Trimmed nonempty value overrides client UA; configured invalid values rejected before startup |
| `localProxyRequestOverrides` | Original `headers` map and optional `body` object; config rejects auth/destination/model overrides |

`settings_config.modelCatalog` supplies the original Zen per-model
`reasoningLevels`/`reasoning_levels` and model media capability declarations.
`settings_config.models` supplies the original alternate model capability registry.
Media declarations live inside model entries (`supportsImage`, `supports_image`,
`vision`, `input`, `modalities.input`, `input_modalities`, `inputModalities`).
Base URL and wire protocol are synthesized into the upstream Provider projection
from the single authoritative base configuration; credentials are never placed
in that projection.

The original `optimizer` contains `enabled`, `thinkingOptimizer`, and
`cacheInjection`. It has **no cache TTL field** at this pinned version. The
Anthropic bridge always injects standard 5-minute cache markers and forces its
own enabled flag while honoring `cacheInjection`.

## Deliberate scope adaptations

- Provider storage is a typed in-memory projection; its optional UI/database
  fields exist only to compile unchanged source test fixtures.
- Configured upstream model selection, Grok Build parsing, Stack routing,
  Copilot normalization, OAuth auth passthrough and Claude inbound conversion
  have no corresponding supported input. They are not simulated with live calls.
- The extracted Codex model helper omits the Grok Build configuration parser;
  public compatibility settings cannot supply a model or raw config string.
- OpenCode's fallback User-Agent keeps `cc-switch/<package version>` syntax,
  with the lightweight binary's package version.
- Config validation is stricter than upstream permissive persistence for
  unsupported options and credentials. Protocol inference functions themselves
  retain the upstream behavior. In particular, the pinned xAI detector uses a
  substring check on its configured URL; it was not silently replaced by a
  different host-matching policy.

Prepared request and HTTP tests exercise production wiring. Fixed local regression
fixtures preserve the accepted behavior of cc-switch-lite commit 58c8c87; ongoing
refactors do not require a separate cc-switch implementation.
