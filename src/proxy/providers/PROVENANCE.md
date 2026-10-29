# Protocol conversion provenance

The forwarding baseline is derived from `cc-switch v4.0.18`.
Migration source: commit `214e75ba7f625c679dd4e61df166ec4463245fe7`, `src-tauri/src/proxy/`,
including the user's local commits noted below.
Copyright (c) 2025 Jason Young, MIT; see `LICENSE-cc-switch`.

Selected fixes from public `cc-switch v4.0.21` (`c4db63c60d2a1ddcb4bd9f81152b9c6126413659`):

- `80640b52`: unwrap non-streaming Chat completion `data` envelopes. The owned
  converter helper is retained; usage-accounting-only helpers are not needed.
- `7f9cd9e9`: use the attached-media marker for orphan tool results.
- `96f01af0`: rectify explicit foreign Azure resource rejections, including item IDs.
- `2e1e2979`: recognize `session-id` and `thread-id` with upstream precedence.

The corresponding upstream regression tests are retained where applicable.
This is a selective port, not a replacement of the baseline or the user's local changes.

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
`docs/compatibility.md` and `docs/request-compatibility.md`.

Local correctness fixes checked against Codex `797bab47` cover incomplete SSE
terminal types, unfinished tool commitment, EOF tail events, namespace custom
tools, and Responses `text.format` mapping. Affected unit expectations are
updated to the client contract; fixed migration fixtures remain unchanged.

Migration was accepted at cc-switch-lite commit `58c8c87`. The source repository
includes the user's local commits, not only public upstream changes. Local tests
and fixed regression fixtures now protect that accepted behavior; the temporary
independent reference package has been removed.
