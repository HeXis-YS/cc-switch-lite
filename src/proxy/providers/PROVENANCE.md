# Protocol conversion provenance

Copied from `cc-switch` commit `214e75ba7f625c679dd4e61df166ec4463245fe7`, `src-tauri/src/proxy/`.
Copyright (c) 2025 Jason Young, MIT; see `LICENSE-cc-switch`.

The conversion, SSE, chat history, tool history, compaction, inline thinking,
canonical JSON, SSE parsing and tool media modules retain their upstream tests.
`transform.rs`, `transform_responses.rs`, `thinking_optimizer.rs`, and
`src/provider.rs` contain only the helper/type dependency closure used by those
converters; unrelated application transforms and optimizer settings are excluded.
The local error enum contains only the two conversion error variants.

Local adaptation: converter warning logs omit request/upstream strings
(call IDs, model names and finish reasons) to keep credentials out of logs.
The compaction module excludes native Responses mutation and OAuth-state scrubbing;
its Chat/Messages helpers and their corresponding tests are retained.
Upstream helper variants used only by retained upstream tests are compiled
under `cfg(test)`; they are not included in the production routing binary.
