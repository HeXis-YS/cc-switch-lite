//! Conservative repair for Anthropic Messages streams that end after a complete
//! `tool_use` input but omit the block and message terminal events.
//!
//! This adapter observes each SSE frame while forwarding the original bytes
//! unchanged. It only appends terminal events after a clean EOF when the open
//! tool input is complete and structurally valid.

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::Value;

use crate::proxy::sse::{strip_sse_field, take_sse_block};

const MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_TOOL_INPUT_JSON_BYTES: usize = 4 * 1024 * 1024;
const MAX_SSE_DELIMITER_BYTES: usize = 4;
const OBSERVE_SLICE_BYTES: usize = 8 * 1024;

#[derive(Debug)]
struct OpenContentBlock {
    index: u64,
    block_type: String,
    start_event: Value,
    input_json: String,
}

#[derive(Debug)]
struct TailState {
    buffer: String,
    utf8_remainder: Vec<u8>,
    valid: bool,
    saw_message_start: bool,
    saw_terminal_event: bool,
    open_block: Option<OpenContentBlock>,
}

impl Default for TailState {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            utf8_remainder: Vec::new(),
            valid: true,
            saw_message_start: false,
            saw_terminal_event: false,
            open_block: None,
        }
    }
}

impl TailState {
    fn disable_repair(&mut self) {
        self.valid = false;
        self.buffer.clear();
        self.utf8_remainder.clear();
        self.open_block = None;
    }

    fn observe(&mut self, bytes: &[u8]) {
        if !self.valid {
            return;
        }

        let mut offset = 0;
        while offset < bytes.len() && self.valid {
            let buffered_bytes = self.buffer.len().saturating_add(self.utf8_remainder.len());
            let buffer_limit = MAX_SSE_EVENT_BYTES + MAX_SSE_DELIMITER_BYTES;
            if buffered_bytes >= buffer_limit {
                self.disable_repair();
                return;
            }

            let available = bytes.len() - offset;
            let room = buffer_limit - buffered_bytes;
            let count = available.min(room).min(OBSERVE_SLICE_BYTES);
            let slice = &bytes[offset..offset + count];
            if !append_utf8_strict(&mut self.buffer, &mut self.utf8_remainder, slice) {
                self.disable_repair();
                return;
            }
            offset += count;

            while let Some(block) = take_sse_block(&mut self.buffer) {
                self.process_event_block(&block);
                if !self.valid {
                    return;
                }
            }

            if self.buffer.len().saturating_add(self.utf8_remainder.len()) > buffer_limit {
                self.disable_repair();
            }
        }
    }

    fn process_event_block(&mut self, block: &str) {
        if block.len() > MAX_SSE_EVENT_BYTES {
            self.disable_repair();
            return;
        }
        if block.trim().is_empty() {
            return;
        }

        let mut event_type = None;
        let mut data_parts = Vec::new();
        for line in block.lines() {
            if line.starts_with(':') {
                continue;
            }
            if let Some(event) = strip_sse_field(line, "event") {
                if event_type.is_some() {
                    self.disable_repair();
                    return;
                }
                event_type = Some(event.trim().to_string());
            } else if let Some(data) = strip_sse_field(line, "data") {
                data_parts.push(data.to_string());
            }
        }

        if data_parts.is_empty() {
            if event_type.is_some() {
                self.disable_repair();
            }
            return;
        }

        let Ok(data) = serde_json::from_str::<Value>(&data_parts.join("\n")) else {
            self.disable_repair();
            return;
        };
        let Some(data_type) = data.get("type").and_then(Value::as_str) else {
            self.disable_repair();
            return;
        };
        if event_type
            .as_deref()
            .is_some_and(|event_type| event_type != data_type)
        {
            self.disable_repair();
            return;
        }

        match data_type {
            "message_start" => {
                if self.saw_message_start
                    || self.open_block.is_some()
                    || !data.get("message").is_some_and(Value::is_object)
                {
                    self.disable_repair();
                    return;
                }
                self.saw_message_start = true;
            }
            "content_block_start" => self.handle_content_block_start(&data),
            "content_block_delta" => self.handle_content_block_delta(&data),
            "content_block_stop" => self.handle_content_block_stop(&data),
            "message_delta" | "message_stop" | "error" => {
                self.saw_terminal_event = true;
            }
            // `ping` is a valid Anthropic keepalive and does not change the
            // content-block lifecycle.
            "ping" => {}
            _ => self.disable_repair(),
        }
    }

    fn handle_content_block_start(&mut self, data: &Value) {
        if !self.saw_message_start || self.open_block.is_some() {
            self.disable_repair();
            return;
        }

        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            self.disable_repair();
            return;
        };
        let Some(block) = data.get("content_block").and_then(Value::as_object) else {
            self.disable_repair();
            return;
        };
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            self.disable_repair();
            return;
        };
        if !matches!(
            block_type,
            "text" | "tool_use" | "thinking" | "redacted_thinking"
        ) {
            self.disable_repair();
            return;
        }

        self.open_block = Some(OpenContentBlock {
            index,
            block_type: block_type.to_string(),
            start_event: data.clone(),
            input_json: String::new(),
        });
    }

    fn handle_content_block_delta(&mut self, data: &Value) {
        if !self.saw_message_start {
            self.disable_repair();
            return;
        }
        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            self.disable_repair();
            return;
        };
        let Some(delta) = data.get("delta").and_then(Value::as_object) else {
            self.disable_repair();
            return;
        };
        let Some(delta_type) = delta.get("type").and_then(Value::as_str) else {
            self.disable_repair();
            return;
        };
        let Some(block) = self.open_block.as_mut() else {
            self.disable_repair();
            return;
        };
        if index != block.index {
            self.disable_repair();
            return;
        }

        match (block.block_type.as_str(), delta_type) {
            ("tool_use", "input_json_delta") => {
                let Some(partial_json) = delta.get("partial_json").and_then(Value::as_str) else {
                    self.disable_repair();
                    return;
                };
                let Some(new_len) = block.input_json.len().checked_add(partial_json.len()) else {
                    self.disable_repair();
                    return;
                };
                if new_len > MAX_TOOL_INPUT_JSON_BYTES {
                    self.disable_repair();
                    return;
                }
                block.input_json.push_str(partial_json);
            }
            ("text", "text_delta") if delta.get("text").is_some_and(Value::is_string) => {}
            ("thinking" | "redacted_thinking", "thinking_delta")
                if delta.get("thinking").is_some_and(Value::is_string) => {}
            ("thinking" | "redacted_thinking", "signature_delta")
                if delta.get("signature").is_some_and(Value::is_string) => {}
            _ => self.disable_repair(),
        }
    }

    fn handle_content_block_stop(&mut self, data: &Value) {
        if !self.saw_message_start {
            self.disable_repair();
            return;
        }
        let Some(index) = data.get("index").and_then(Value::as_u64) else {
            self.disable_repair();
            return;
        };
        if self
            .open_block
            .as_ref()
            .is_none_or(|block| block.index != index)
        {
            self.disable_repair();
            return;
        }
        self.open_block = None;
    }

    fn finish(mut self) -> Option<Bytes> {
        if !self.valid || !self.utf8_remainder.is_empty() {
            return None;
        }

        let mut separator = "";
        if !self.buffer.is_empty() {
            let final_block = std::mem::take(&mut self.buffer);
            separator = if final_block.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            self.process_event_block(&final_block);
        }

        if !self.valid || !self.saw_message_start || self.saw_terminal_event {
            return None;
        }
        let block = self.open_block?;
        if block.block_type != "tool_use" || block.input_json.is_empty() {
            return None;
        }

        let content_block = block.start_event.get("content_block")?;
        if !content_block
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.trim().is_empty())
            || !content_block
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.trim().is_empty())
            || !content_block.get("input").is_some_and(Value::is_object)
        {
            return None;
        }
        let Ok(parsed_input) = serde_json::from_str::<Value>(&block.input_json) else {
            return None;
        };
        if !parsed_input.is_object() {
            return None;
        }

        Some(Bytes::from(format!(
            "{separator}event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":0}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            block.index
        )))
    }
}

/// Incrementally inspect Anthropic SSE while preserving every upstream chunk.
/// Terminal events are appended only after a clean EOF and a validated open tool.
pub(crate) fn repair_anthropic_sse_tail<S, E>(
    stream: S,
) -> impl Stream<Item = Result<Bytes, E>> + Send
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: Send + 'static,
{
    async_stream::stream! {
        let mut state = TailState::default();
        tokio::pin!(stream);

        while let Some(item) = stream.next().await {
            match &item {
                Ok(bytes) => state.observe(bytes),
                Err(_) => state.disable_repair(),
            }
            yield item;
        }

        if let Some(repaired_tail) = state.finish() {
            yield Ok(repaired_tail);
        }
    }
}

fn append_utf8_strict(buffer: &mut String, remainder: &mut Vec<u8>, bytes: &[u8]) -> bool {
    if remainder.is_empty() {
        return append_utf8_prefix(buffer, remainder, bytes);
    }

    let mut combined = std::mem::take(remainder);
    combined.extend_from_slice(bytes);
    append_utf8_prefix(buffer, remainder, &combined)
}

fn append_utf8_prefix(buffer: &mut String, remainder: &mut Vec<u8>, bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            buffer.push_str(text);
            true
        }
        Err(error) => {
            let valid_up_to = error.valid_up_to();
            if let Ok(valid) = std::str::from_utf8(&bytes[..valid_up_to]) {
                buffer.push_str(valid);
            }
            if error.error_len().is_some() {
                false
            } else {
                remainder.extend_from_slice(&bytes[valid_up_to..]);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::repair_anthropic_sse_tail;
    use crate::proxy::providers::{
        streaming_codex_anthropic::create_responses_sse_stream_from_anthropic_with_context,
        transform_codex_chat::CodexToolContext,
    };
    use bytes::Bytes;
    use futures::{stream, Stream, StreamExt};
    use serde_json::{json, Value};
    use std::io;

    fn event(event_type: &str, data: Value) -> String {
        format!(
            "event: {event_type}\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap()
        )
    }

    fn message_start() -> String {
        event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_tail_test",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-test",
                    "content": [],
                    "usage": { "input_tokens": 1, "output_tokens": 0 }
                }
            }),
        )
    }

    fn tool_start(block: Value) -> String {
        event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": block
            }),
        )
    }

    fn tool_delta(partial_json: &str) -> String {
        event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": partial_json
                }
            }),
        )
    }

    fn valid_tool_prefix(partial_json: &str) -> String {
        format!(
            "{}{}{}",
            message_start(),
            tool_start(json!({
                "type": "tool_use",
                "id": "toolu_tail_test",
                "name": "weather",
                "input": {}
            })),
            tool_delta(partial_json)
        )
    }

    async fn collect_raw(
        stream: impl Stream<Item = Result<Bytes, io::Error>>,
    ) -> Result<Vec<u8>, io::Error> {
        futures::pin_mut!(stream);
        let mut output = Vec::new();
        while let Some(item) = stream.next().await {
            output.extend_from_slice(&item?);
        }
        Ok(output)
    }

    async fn collect_text(stream: impl Stream<Item = Result<Bytes, io::Error>>) -> String {
        String::from_utf8(collect_raw(stream).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn clean_eof_repairs_a_complete_tool_and_codex_commits_it() {
        let input = valid_tool_prefix(r#"{"city":"Tokyo"}"#);
        let repaired = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
            _,
            io::Error,
        >(
            Bytes::copy_from_slice(input.as_bytes()),
        )])))
        .await
        .unwrap();
        let repaired = String::from_utf8(repaired).unwrap();

        assert!(repaired.starts_with(&input));
        assert!(repaired.ends_with(concat!(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":0}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        )));

        let converted = create_responses_sse_stream_from_anthropic_with_context(
            repair_anthropic_sse_tail(stream::iter(vec![Ok::<_, io::Error>(Bytes::from(input))])),
            CodexToolContext::default(),
        );
        let converted = collect_text(converted).await;
        assert!(converted.contains("event: response.function_call_arguments.done"));
        assert!(converted.contains("event: response.output_item.done"));
        assert!(converted.contains("Tokyo"));
        assert!(converted.contains("event: response.completed"));
        assert!(!converted.contains("event: response.incomplete"));
    }

    #[tokio::test]
    async fn complete_stream_is_forwarded_byte_for_byte() {
        let mut input = valid_tool_prefix(r#"{"city":"Tokyo"}"#);
        input.push_str(&event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ));
        input.push_str(&event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" },
                "usage": { "output_tokens": 2 }
            }),
        ));
        input.push_str(&event("message_stop", json!({ "type": "message_stop" })));

        let output = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
            _,
            io::Error,
        >(
            Bytes::copy_from_slice(input.as_bytes()),
        )])))
        .await
        .unwrap();
        assert_eq!(output, input.as_bytes());
    }

    #[tokio::test]
    async fn fragmented_utf8_and_unterminated_eof_event_are_repaired() {
        let input = valid_tool_prefix(r#"{"city":"東京"}"#)
            .trim_end_matches("\n\n")
            .to_string();
        let chunks = input
            .as_bytes()
            .chunks(1)
            .map(|chunk| Ok::<_, io::Error>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();

        let output = collect_raw(repair_anthropic_sse_tail(stream::iter(chunks)))
            .await
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(&input));
        assert!(output.contains("event: content_block_stop"));
        assert!(output.contains("\"index\":0"));
        assert!(output.contains("\"output_tokens\":0"));
    }

    #[tokio::test]
    async fn crlf_frames_are_repaired() {
        let input = valid_tool_prefix(r#"{"ok":true}"#)
            .trim_end_matches("\n\n")
            .replace('\n', "\r\n");
        let output = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
            _,
            io::Error,
        >(
            Bytes::copy_from_slice(input.as_bytes()),
        )])))
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(&input));
        assert!(output.contains("\r\n\r\n"));
        assert!(output.contains("event: content_block_stop\n"));
    }

    #[tokio::test]
    async fn structurally_incomplete_or_ambiguous_streams_are_not_repaired() {
        let valid_delta = r#"{"city":"Tokyo"}"#;
        let message_delta = event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "tool_use" }
            }),
        );
        let message_stop = event("message_stop", json!({ "type": "message_stop" }));
        let upstream_error = event(
            "error",
            json!({
                "type": "error",
                "error": { "type": "overloaded_error", "message": "busy" }
            }),
        );
        let mismatched_event = event(
            "content_block_stop",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": valid_delta }
            }),
        );

        let cases = [
            ("invalid JSON", valid_tool_prefix(r#"{"city":}"#)),
            ("incomplete JSON", valid_tool_prefix(r#"{"city":"Tokyo""#)),
            ("non-object JSON", valid_tool_prefix("[]")),
            (
                "missing tool id",
                format!(
                    "{}{}{}",
                    message_start(),
                    tool_start(json!({ "type": "tool_use", "name": "weather", "input": {} })),
                    tool_delta(valid_delta)
                ),
            ),
            (
                "missing tool name",
                format!(
                    "{}{}{}",
                    message_start(),
                    tool_start(json!({ "type": "tool_use", "id": "toolu_1", "input": {} })),
                    tool_delta(valid_delta)
                ),
            ),
            (
                "non-object start input",
                format!(
                    "{}{}{}",
                    message_start(),
                    tool_start(json!({
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "weather",
                        "input": "not an object"
                    })),
                    tool_delta(valid_delta)
                ),
            ),
            (
                "no streamed tool input",
                format!(
                    "{}{}",
                    message_start(),
                    tool_start(json!({
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "weather",
                        "input": { "city": "Tokyo" }
                    }))
                ),
            ),
            (
                "open text block",
                format!(
                    "{}{}",
                    message_start(),
                    tool_start(json!({ "type": "text", "text": "hello" }))
                ),
            ),
            (
                "existing message delta",
                format!("{}{}", valid_tool_prefix(valid_delta), message_delta),
            ),
            (
                "existing message stop",
                format!("{}{}", valid_tool_prefix(valid_delta), message_stop),
            ),
            (
                "upstream error event",
                format!("{}{}", valid_tool_prefix(valid_delta), upstream_error),
            ),
            (
                "event and data type mismatch",
                format!("{}{}", message_start(), mismatched_event),
            ),
            (
                "malformed JSON data",
                format!(
                    "{}{}event: content_block_delta\ndata: {{\"type\":\n\n",
                    message_start(),
                    tool_start(json!({
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "weather",
                        "input": {}
                    }))
                ),
            ),
        ];

        for (name, input) in cases {
            let output = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
                _,
                io::Error,
            >(
                Bytes::copy_from_slice(input.as_bytes()),
            )])))
            .await
            .unwrap();
            assert_eq!(output, input.as_bytes(), "case: {name}");
        }
    }

    #[tokio::test]
    async fn incomplete_json_does_not_commit_a_codex_tool_call() {
        let input = valid_tool_prefix(r#"{"city":"Tokyo""#);
        let converted = create_responses_sse_stream_from_anthropic_with_context(
            repair_anthropic_sse_tail(stream::iter(vec![Ok::<_, io::Error>(Bytes::from(input))])),
            CodexToolContext::default(),
        );
        let converted = collect_text(converted).await;
        assert!(converted.contains("event: response.incomplete"));
        assert!(!converted.contains("event: response.output_item.done"));
        assert!(!converted.contains("event: response.function_call_arguments.done"));
    }

    #[tokio::test]
    async fn transport_error_is_forwarded_and_never_repairs_the_tail() {
        let input = valid_tool_prefix(r#"{"city":"Tokyo"}"#);
        let source = stream::iter(vec![
            Ok::<_, io::Error>(Bytes::copy_from_slice(input.as_bytes())),
            Err(io::Error::other("connection reset")),
        ]);
        let mut repaired = Box::pin(repair_anthropic_sse_tail(source));

        assert_eq!(
            repaired.next().await.unwrap().unwrap().as_ref(),
            input.as_bytes()
        );
        assert_eq!(
            repaired.next().await.unwrap().unwrap_err().to_string(),
            "connection reset"
        );
        assert!(repaired.next().await.is_none());
    }

    #[tokio::test]
    async fn oversized_sse_event_is_forwarded_without_repair() {
        let mut input = valid_tool_prefix("{");
        let oversized_delta = "x".repeat(super::MAX_SSE_EVENT_BYTES + 64);
        input.push_str(&event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "input_json_delta",
                    "partial_json": oversized_delta
                }
            }),
        ));

        let output = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
            _,
            io::Error,
        >(
            Bytes::copy_from_slice(input.as_bytes()),
        )])))
        .await
        .unwrap();
        assert_eq!(output, input.as_bytes());
    }

    #[tokio::test]
    async fn oversized_tool_input_is_forwarded_without_repair() {
        let mut input = valid_tool_prefix("{\"value\":\"");
        let fragment = "a".repeat(900_000);
        for _ in 0..5 {
            input.push_str(&tool_delta(&fragment));
        }
        input.push_str(&tool_delta("\"}"));

        let output = collect_raw(repair_anthropic_sse_tail(stream::iter(vec![Ok::<
            _,
            io::Error,
        >(
            Bytes::copy_from_slice(input.as_bytes()),
        )])))
        .await
        .unwrap();
        assert_eq!(output, input.as_bytes());
    }
}
