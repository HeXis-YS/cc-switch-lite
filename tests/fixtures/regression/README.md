# Accepted behavior fixtures

Captured from cc-switch-lite `58c8c87` before production refactoring. Inputs live
in `tests/regression.rs`; each JSON array follows its corresponding case order.
SSE files are exact output bytes shared by all three fragmentation sizes.
Only generated OpenCode session UUIDs use the existing normalization exception.
All keys and values in these fixtures are synthetic test data.

These are fixed expectations, not automatically updated snapshots. Tests never
write them. The temporary capture code was removed after the baseline assertions
passed. Migration acceptance is complete; no reference implementation is needed.
