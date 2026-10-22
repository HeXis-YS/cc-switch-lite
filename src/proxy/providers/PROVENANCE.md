# Protocol conversion provenance

Copied from `cc-switch` commit `214e75ba7f625c679dd4e61df166ec4463245fe7`, `src-tauri/src/proxy/`.
Copyright (c) 2025 Jason Young, MIT; see `LICENSE-cc-switch`.

The conversion, SSE, chat history, tool history, compaction, inline thinking,
canonical JSON, SSE parsing and tool media modules retain their upstream tests.
The closure also includes native Responses compaction, namespace/xAI and Moonshot
schema handling, late arguments, media and opaque-state rectification, body
filtering, cache injection, content decoding, session helpers and error mapping.
The compaction source retains official-provider scrub helpers and tests, although
the API-key routing path does not invoke official OAuth behavior.

`transform.rs`, `transform_responses.rs`, `thinking_optimizer.rs`, and
`src/provider.rs` retain the helper/type closure needed by these paths. The error
module retains upstream variants and response mappings. Application infrastructure
is replaced by configuration snapshots and the single-provider HTTP handler.

Local adaptation: converter warning logs omit request/upstream strings
(call IDs, model names and finish reasons) to keep credentials out of logs.
Request/response orchestration and necessary adaptations are documented in
`docs/compatibility.md` and `docs/request-compatibility.md`. The independently
vendored reference package records exact source paths and hashes in
`reference/cc-switch-reference/provenance.json`; it is used for differential tests,
not linked into the production binary.
