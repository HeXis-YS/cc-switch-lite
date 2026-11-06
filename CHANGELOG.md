# Changelog

## Unreleased

- Removed native Gemini provider support. Configurations using `protocol: gemini_native` now fail validation. To keep using a Gemini model through an OpenAI-compatible service, configure that service as `chat_completions`, use its Chat Completions endpoint and authentication settings, and remove the `compat.gemini` block.
