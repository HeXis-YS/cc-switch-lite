//! Gemini Native streaming conversion module.
//!
//! Converts Gemini `streamGenerateContent?alt=sse` chunks directly into
//! OpenAI Responses SSE events.

// Derived from cc-switch@214e75ba7f625c679dd4e61df166ec4463245fe7.
// Copyright (c) 2025 Jason Young. MIT; see providers/LICENSE-cc-switch.

use super::codex_responses_sse as sse;
use super::gemini_shadow::{GeminiShadowStore, GeminiToolCallMeta};
use super::transform_codex_chat::{
    custom_tool_input_from_chat_arguments, response_tool_call_item_from_chat_name,
    response_tool_call_item_id_from_chat_name, CodexToolContext,
};
use super::transform_gemini::{
    build_responses_usage, gemini_web_search_call_id, gemini_web_search_call_item,
    is_synthesized_tool_call_id, map_gemini_finish_reason, rectify_tool_call_parts, response_id,
    synthesize_tool_call_id, GeminiToolSchemaHints,
};
use crate::proxy::json_canonical::canonical_json_string;
use crate::proxy::sse::{append_utf8_safe, strip_sse_field, take_sse_block};
use bytes::Bytes;
use futures::stream::{self, Stream, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;

fn extract_visible_text(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|part| part.get("thought").and_then(|value| value.as_bool()) != Some(true))
        .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
        .collect::<String>()
}

fn extract_tool_calls(
    parts: &[Value],
    tool_schema_hints: Option<&GeminiToolSchemaHints>,
) -> Vec<GeminiToolCallMeta> {
    let mut rectified_parts = parts.to_vec();
    rectify_tool_call_parts(&mut rectified_parts, tool_schema_hints);

    rectified_parts
        .iter()
        .filter_map(|part| {
            let function_call = part.get("functionCall")?;
            // Treat an explicit empty-string id as equivalent to a missing
            // one. Some Gemini relays serialize absent ids as `"id": ""`;
            // without this filter the `Some("")` value would flow into
            // `merge_tool_call_snapshots`, match itself across chunks, and
            // collapse parallel no-id calls into a single snapshot with an
            // empty-string function-call id.
            let id = function_call
                .get("id")
                .and_then(|value| value.as_str())
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            Some(GeminiToolCallMeta::new(
                id,
                function_call
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                function_call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
                part.get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(|value| value.as_str()),
            ))
        })
        .collect()
}

fn extract_text_thought_signature(parts: &[Value]) -> Option<String> {
    parts
        .iter()
        .filter(|part| part.get("text").is_some() && part.get("functionCall").is_none())
        .filter_map(|part| {
            part.get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(|value| value.as_str())
        })
        .next_back()
        .map(ToString::to_string)
}

fn merge_tool_call_snapshots(
    tool_call_snapshots: &mut Vec<GeminiToolCallMeta>,
    incoming: Vec<GeminiToolCallMeta>,
) {
    // Gemini's `streamGenerateContent?alt=sse` delivers each chunk as the
    // cumulative snapshot of `content.parts`. For the same tool call across
    // chunks we therefore need to map an incoming entry back to whichever
    // snapshot entry it describes:
    //
    // 1. If both sides carry a genuine Gemini id, match by id.
    // 2. Otherwise match by position in the cumulative `parts` array — this
    //    is how parallel no-id calls stay distinguishable.
    //
    // A previous implementation fell back to matching by `name`, which silently
    // merged two parallel calls to the same function into one entry (losing
    // the first call's args). That fallback is removed here.
    for (position, mut tool_call) in incoming.into_iter().enumerate() {
        // Treat an empty-string id as "missing" throughout this function.
        // `extract_tool_calls` already filters `""` at the source, but upstream
        // callers that build `GeminiToolCallMeta` by hand (tests, future code)
        // could still send `Some("")` — collapsing it here keeps the invariant
        // local to this merge step.
        if tool_call.id.as_deref() == Some("") {
            tool_call.id = None;
        }

        let existing_index = match tool_call.id.as_deref() {
            Some(incoming_id) => tool_call_snapshots
                .iter()
                .position(|existing| existing.id.as_deref() == Some(incoming_id))
                .or_else(|| {
                    // Fallback for the "synth -> real id upgrade" case:
                    // Gemini's cumulative stream may deliver the first chunk
                    // of a tool call without an id (we synthesize one) and
                    // then upgrade it to a genuine id on a later chunk. A
                    // pure id-match would miss the existing synthesized
                    // snapshot and push a second entry, yielding duplicate
                    // Responses function-call items at stream end. If the
                    // same-position slot currently holds a synthesized id,
                    // merge into it — `or(preserved_id)` below will keep
                    // the real id, dropping the synthesized one.
                    tool_call_snapshots
                        .get(position)
                        .filter(|existing| {
                            matches!(
                                existing.id.as_deref(),
                                Some(id) if is_synthesized_tool_call_id(id)
                            )
                        })
                        .map(|_| position)
                }),
            None => tool_call_snapshots
                .get(position)
                .filter(|existing| match existing.id.as_deref() {
                    // Only merge into a positional match when the prior
                    // snapshot was itself id-less (or we synthesized one).
                    // A snapshot with a genuine Gemini id at this index is
                    // treated as a different call — the incoming entry gets
                    // its own synthesized id below.
                    Some(id) => is_synthesized_tool_call_id(id),
                    None => true,
                })
                .map(|_| position),
        };

        if let Some(index) = existing_index {
            // Preserve any synthesized id assigned on a previous chunk so the
            // Responses-visible id stays stable across the whole stream.
            // When incoming carries a real Gemini id and the slot holds a
            // synthesized one, `Some(real).or(Some(synth)) == Some(real)`
            // so the upgrade wins naturally.
            let preserved_id = tool_call_snapshots[index].id.clone();
            tool_call.id = tool_call.id.or(preserved_id);

            // Preserve `thought_signature` across chunks. Gemini's cumulative
            // stream may include `thoughtSignature` on one chunk and omit it
            // on a subsequent cumulative snapshot of the same part, even
            // though the signature still belongs to the call. A blind
            // overwrite would drop it, so the shadow turn we record (and
            // later replay) would be missing `thoughtSignature` and the
            // upstream would reject the follow-up for invalid signature.
            if tool_call.thought_signature.is_none() {
                tool_call
                    .thought_signature
                    .clone_from(&tool_call_snapshots[index].thought_signature);
            }
        }
        if tool_call.id.is_none() {
            tool_call.id = Some(synthesize_tool_call_id());
        }

        match existing_index {
            Some(index) => tool_call_snapshots[index] = tool_call,
            None => tool_call_snapshots.push(tool_call),
        }
    }
}

fn build_shadow_assistant_parts(
    text: Option<&str>,
    text_thought_signature: Option<&str>,
    tool_calls: &[GeminiToolCallMeta],
) -> Vec<Value> {
    let mut parts = Vec::new();

    if text.filter(|text| !text.is_empty()).is_some() || text_thought_signature.is_some() {
        let mut part = json!({
            "text": text.unwrap_or("")
        });
        if let Some(signature) = text_thought_signature {
            part["thoughtSignature"] = json!(signature);
        }
        parts.push(part);
    }

    for tool_call in tool_calls {
        let mut part = json!({
            "functionCall": {
                "id": tool_call.id.clone().unwrap_or_default(),
                "name": tool_call.name,
                "args": tool_call.args
            }
        });

        if let Some(signature) = &tool_call.thought_signature {
            part["thoughtSignature"] = json!(signature);
        }

        parts.push(part);
    }

    parts
}

#[derive(Debug, Default)]
struct GeminiTextState {
    output_index: Option<u32>,
    item_id: String,
    text: String,
    added: bool,
    done: bool,
}

#[derive(Debug)]
struct GeminiToResponsesState {
    response_started: bool,
    completed: bool,
    response_id: String,
    gemini_response_id: Option<String>,
    model: String,
    next_output_index: u32,
    text: GeminiTextState,
    output_items: Vec<(u32, Value)>,
    tool_call_snapshots: Vec<GeminiToolCallMeta>,
    seen_web_search_queries: HashSet<String>,
    pending_web_search_queries: Vec<(String, String)>,
    next_web_search_index: usize,
    text_thought_signature: Option<String>,
    latest_usage: Option<Value>,
    latest_finish_reason: Option<String>,
    blocked_reason: Option<String>,
    saw_done_marker: bool,
    shadow_store: Option<Arc<GeminiShadowStore>>,
    provider_id: Option<String>,
    session_id: Option<String>,
    tool_schema_hints: Option<GeminiToolSchemaHints>,
    tool_context: CodexToolContext,
}

impl GeminiToResponsesState {
    fn new(
        shadow_store: Option<Arc<GeminiShadowStore>>,
        provider_id: Option<String>,
        session_id: Option<String>,
        tool_schema_hints: Option<GeminiToolSchemaHints>,
        tool_context: CodexToolContext,
        model_fallback: String,
    ) -> Self {
        Self {
            response_started: false,
            completed: false,
            response_id: "resp_ccswitch".to_string(),
            gemini_response_id: None,
            model: model_fallback,
            next_output_index: 0,
            text: GeminiTextState::default(),
            output_items: Vec::new(),
            tool_call_snapshots: Vec::new(),
            seen_web_search_queries: HashSet::new(),
            pending_web_search_queries: Vec::new(),
            next_web_search_index: 0,
            text_thought_signature: None,
            latest_usage: None,
            latest_finish_reason: None,
            blocked_reason: None,
            saw_done_marker: false,
            shadow_store,
            provider_id,
            session_id,
            tool_schema_hints,
            tool_context,
        }
    }

    fn handle_gemini_chunk(&mut self, chunk: &Value) -> Vec<Bytes> {
        if !self.response_started {
            if let Some(id) = chunk
                .get("responseId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                self.gemini_response_id = Some(id.to_string());
                self.response_id = response_id(Some(id));
            }
            if let Some(model) = chunk
                .get("modelVersion")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
            {
                self.model = model.to_string();
            }
        }
        if let Some(usage) = chunk.get("usageMetadata") {
            self.latest_usage = Some(usage.clone());
        }

        let mut events = self.ensure_response_started();
        if let Some(reason) = chunk
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            self.blocked_reason = Some(reason.to_string());
        }

        let Some(candidate) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
        else {
            return events;
        };

        if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
            self.latest_finish_reason = Some(reason.to_string());
        }
        if self.blocked_reason.is_none() && self.tool_context.hosted_web_search().is_some() {
            for query in super::transform_gemini::extract_gemini_web_search_queries(candidate) {
                if !self.seen_web_search_queries.insert(query.clone()) {
                    continue;
                }
                let call_id = gemini_web_search_call_id(
                    self.gemini_response_id.as_deref(),
                    self.next_web_search_index,
                );
                self.next_web_search_index += 1;
                if self.text.added {
                    self.pending_web_search_queries.push((call_id, query));
                } else {
                    events.extend(self.push_web_search_call(&call_id, &query));
                }
            }
        }

        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(Value::as_array)
        {
            let mut rectified_parts = parts.clone();
            rectify_tool_call_parts(&mut rectified_parts, self.tool_schema_hints.as_ref());
            if let Some(signature) = extract_text_thought_signature(parts) {
                self.text_thought_signature = Some(signature);
            }
            merge_tool_call_snapshots(
                &mut self.tool_call_snapshots,
                extract_tool_calls(&rectified_parts, self.tool_schema_hints.as_ref()),
            );

            let visible_text = extract_visible_text(&rectified_parts);
            if !visible_text.is_empty() {
                let delta = if visible_text.starts_with(&self.text.text) {
                    visible_text[self.text.text.len()..].to_string()
                } else {
                    visible_text
                };
                if !delta.is_empty() {
                    events.extend(self.push_text_delta(&delta));
                }
            }
        }

        events
    }

    fn ensure_response_started(&mut self) -> Vec<Bytes> {
        if self.response_started {
            return Vec::new();
        }
        self.response_started = true;
        let response = self.base_response("in_progress", Vec::new());
        vec![
            sse::response_created(&response),
            sse::response_in_progress(&response),
        ]
    }

    fn push_text_delta(&mut self, delta: &str) -> Vec<Bytes> {
        let mut events = Vec::new();
        if !self.text.added {
            let output_index = self.next_output_index();
            let item_id = format!("{}_msg_0", self.response_id);
            self.text.output_index = Some(output_index);
            self.text.item_id.clone_from(&item_id);
            self.text.added = true;
            events.push(sse::message_item_added(output_index, &item_id));
            events.push(sse::message_content_part_added(output_index, &item_id));
        }
        self.text.text.push_str(delta);
        events.push(sse::output_text_delta(
            self.text.output_index.unwrap_or(0),
            &self.text.item_id,
            delta,
        ));
        events
    }

    fn push_web_search_call(&mut self, call_id: &str, query: &str) -> Vec<Bytes> {
        let output_index = self.next_output_index();
        let item_id = format!("ws_{call_id}");
        let item = gemini_web_search_call_item(&item_id, "completed", query);
        let mut added = item.clone();
        added["status"] = json!("in_progress");
        self.output_items.push((output_index, item.clone()));
        vec![
            sse::output_item_added(output_index, &added),
            sse::output_item_done(output_index, &item),
        ]
    }

    fn finish_text(&mut self) -> Vec<Bytes> {
        if !self.text.added || self.text.done {
            return Vec::new();
        }
        let output_index = self.text.output_index.unwrap_or(0);
        let (events, item) = sse::message_close(output_index, &self.text.item_id, &self.text.text);
        self.output_items.push((output_index, item));
        self.text.done = true;
        events
    }

    fn finish_tool_calls(&mut self) -> Vec<Bytes> {
        let mut events = Vec::new();
        for call in self.tool_call_snapshots.clone() {
            let call_id = call
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(ToString::to_string)
                .unwrap_or_else(synthesize_tool_call_id);
            let arguments = canonical_json_string(&call.args);
            let item_id =
                response_tool_call_item_id_from_chat_name(&call_id, &call.name, &self.tool_context);
            let item = response_tool_call_item_from_chat_name(
                &item_id,
                "completed",
                &call_id,
                &call.name,
                &arguments,
                None,
                &self.tool_context,
            );
            let added = response_tool_call_item_from_chat_name(
                &item_id,
                "in_progress",
                &call_id,
                &call.name,
                "",
                None,
                &self.tool_context,
            );
            let output_index = self.next_output_index();
            events.push(sse::output_item_added(output_index, &added));

            if self.tool_context.is_custom_tool_chat_name(&call.name) {
                let input = custom_tool_input_from_chat_arguments(&arguments);
                if !input.is_empty() {
                    events.push(sse::custom_tool_call_input_delta(
                        output_index,
                        &item_id,
                        &input,
                    ));
                }
                events.push(sse::custom_tool_call_input_done(
                    output_index,
                    &item_id,
                    &input,
                ));
            } else {
                events.push(sse::function_call_arguments_delta(
                    output_index,
                    &item_id,
                    &arguments,
                ));
                events.push(sse::function_call_arguments_done(
                    output_index,
                    &item_id,
                    &arguments,
                ));
            }
            events.push(sse::output_item_done(output_index, &item));
            self.output_items.push((output_index, item));
        }
        events
    }

    fn finish(&mut self) -> Vec<Bytes> {
        if self.completed {
            return Vec::new();
        }
        let mut events = self.ensure_response_started();
        if self.text.text.is_empty() {
            if let Some(reason) = self.blocked_reason.as_deref() {
                let text = format!("Request blocked by Gemini safety filters: {reason}");
                events.extend(self.push_text_delta(&text));
            }
        }
        events.extend(self.finish_text());

        if self.blocked_reason.is_none() {
            for (call_id, query) in std::mem::take(&mut self.pending_web_search_queries) {
                events.extend(self.push_web_search_call(&call_id, &query));
            }
        }

        let (status, incomplete_reason) = if self.blocked_reason.is_some() {
            ("incomplete", Some("content_filter"))
        } else if self.latest_finish_reason.is_none() {
            ("incomplete", Some("max_output_tokens"))
        } else {
            map_gemini_finish_reason(self.latest_finish_reason.as_deref())
        };
        if status == "completed" && self.latest_finish_reason.as_deref() == Some("STOP") {
            self.record_shadow_turn();
        }
        if status == "completed" && self.blocked_reason.is_none() {
            events.extend(self.finish_tool_calls());
        }

        let mut response = self.base_response(status, self.completed_output_items());
        if let Some(reason) = incomplete_reason {
            response["incomplete_details"] = json!({ "reason": reason });
            events.push(sse::response_incomplete(&response));
        } else {
            events.push(sse::response_completed(&response));
        }
        self.completed = true;
        events
    }

    fn record_shadow_turn(&self) {
        let (Some(store), Some(provider_id), Some(session_id)) = (
            self.shadow_store.as_ref(),
            self.provider_id.as_deref(),
            self.session_id.as_deref(),
        ) else {
            return;
        };
        let text = (!self.text.text.is_empty()).then_some(self.text.text.as_str());
        let shadow_parts = build_shadow_assistant_parts(
            text,
            self.text_thought_signature.as_deref(),
            &self.tool_call_snapshots,
        );
        if !shadow_parts.is_empty() {
            store.record_assistant_turn(
                provider_id,
                session_id,
                json!({ "parts": shadow_parts }),
                self.tool_call_snapshots.clone(),
            );
        }
    }

    fn completed_output_items(&self) -> Vec<Value> {
        let mut output_items = self.output_items.clone();
        output_items.sort_by_key(|(output_index, _)| *output_index);
        output_items.into_iter().map(|(_, item)| item).collect()
    }

    fn base_response(&self, status: &str, output: Vec<Value>) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": 0,
            "status": status,
            "model": self.model,
            "output": output,
            "usage": build_responses_usage(self.latest_usage.as_ref())
        })
    }

    fn next_output_index(&mut self) -> u32 {
        let index = self.next_output_index;
        self.next_output_index += 1;
        index
    }

    fn process_sse_block(&mut self, block: &str) -> Vec<Bytes> {
        let mut data_lines = Vec::new();
        for line in block.lines() {
            if let Some(data) = strip_sse_field(line, "data") {
                data_lines.push(data.to_string());
            }
        }
        if data_lines.is_empty() {
            return Vec::new();
        }
        let data = data_lines.join("\n");
        if data.trim() == "[DONE]" {
            self.saw_done_marker = true;
            return Vec::new();
        }
        match serde_json::from_str::<Value>(&data) {
            Ok(chunk) => self.handle_gemini_chunk(&chunk),
            Err(_) => Vec::new(),
        }
    }
}

pub(crate) fn create_responses_sse_stream_from_gemini<E: std::error::Error + Send + 'static>(
    stream: impl Stream<Item = Result<Bytes, E>> + Send + 'static,
    shadow_store: Option<Arc<GeminiShadowStore>>,
    provider_id: Option<String>,
    session_id: Option<String>,
    tool_schema_hints: Option<GeminiToolSchemaHints>,
    tool_context: CodexToolContext,
    model_fallback: String,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        let mut buffer = String::new();
        let mut utf8_remainder = Vec::new();
        let mut state = GeminiToResponsesState::new(
            shadow_store,
            provider_id,
            session_id,
            tool_schema_hints,
            tool_context,
            model_fallback,
        );
        let stream = stream.chain(stream::iter([Ok::<Bytes, E>(Bytes::from_static(b"\n\n"))]));
        tokio::pin!(stream);

        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    append_utf8_safe(&mut buffer, &mut utf8_remainder, &bytes);
                    while let Some(block) = take_sse_block(&mut buffer) {
                        for event in state.process_sse_block(&block) {
                            yield Ok(event);
                        }
                        if state.saw_done_marker {
                            break;
                        }
                    }
                    if state.saw_done_marker {
                        break;
                    }
                }
                Err(error) => {
                    yield Err(std::io::Error::other(error.to_string()));
                    return;
                }
            }
        }

        for event in state.process_sse_block(&buffer) {
            yield Ok(event);
        }
        for event in state.finish() {
            yield Ok(event);
        }
    }
}

pub(crate) fn responses_sse_events_from_gemini_response(
    response: &Value,
    tool_context: &CodexToolContext,
) -> Vec<Bytes> {
    let mut events = Vec::new();
    let response_id = response
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("resp_ccswitch");
    let mut in_progress = response.clone();
    in_progress["status"] = json!("in_progress");
    in_progress["output"] = json!([]);
    events.push(sse::response_created(&in_progress));
    events.push(sse::response_in_progress(&in_progress));

    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for (output_index, item) in output.iter().enumerate() {
            let output_index = output_index as u32;
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            match item_type {
                "message" => {
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or(response_id);
                    let mut added = item.clone();
                    added["status"] = json!("in_progress");
                    added["content"] = json!([]);
                    events.push(sse::output_item_added(output_index, &added));
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for (content_index, part) in parts.iter().enumerate() {
                            let content_index = content_index as u32;
                            let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
                            if part_type != "output_text" {
                                continue;
                            }
                            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                            let mut empty_part = part.clone();
                            empty_part["text"] = json!("");
                            events.push(sse::sse_event(
                                "response.content_part.added",
                                json!({
                                    "type": "response.content_part.added",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": empty_part
                                }),
                            ));
                            events.push(sse::sse_event(
                                "response.output_text.delta",
                                json!({
                                    "type": "response.output_text.delta",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "delta": text
                                }),
                            ));
                            events.push(sse::sse_event(
                                "response.output_text.done",
                                json!({
                                    "type": "response.output_text.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "text": text
                                }),
                            ));
                            events.push(sse::sse_event(
                                "response.content_part.done",
                                json!({
                                    "type": "response.content_part.done",
                                    "item_id": item_id,
                                    "output_index": output_index,
                                    "content_index": content_index,
                                    "part": part
                                }),
                            ));
                        }
                    }
                    events.push(sse::output_item_done(output_index, item));
                }
                "function_call" | "custom_tool_call" | "tool_search_call" => {
                    let mut added = item.clone();
                    added["status"] = json!("in_progress");
                    match item_type {
                        "function_call" => added["arguments"] = json!(""),
                        "custom_tool_call" => added["input"] = json!(""),
                        "tool_search_call" => added["arguments"] = json!({}),
                        _ => {}
                    }
                    events.push(sse::output_item_added(output_index, &added));
                    if item_type == "custom_tool_call" {
                        let input = item.get("input").and_then(Value::as_str).unwrap_or("");
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                        if !input.is_empty() {
                            events.push(sse::custom_tool_call_input_delta(
                                output_index,
                                item_id,
                                input,
                            ));
                        }
                        events.push(sse::custom_tool_call_input_done(
                            output_index,
                            item_id,
                            input,
                        ));
                    } else {
                        let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                        let item_id = item
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                if item_type == "tool_search_call" {
                                    response_tool_call_item_id_from_chat_name(
                                        call_id,
                                        "tool_search",
                                        tool_context,
                                    )
                                } else {
                                    String::new()
                                }
                            });
                        let arguments = item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        events.push(sse::function_call_arguments_delta(
                            output_index,
                            &item_id,
                            arguments,
                        ));
                        events.push(sse::function_call_arguments_done(
                            output_index,
                            &item_id,
                            arguments,
                        ));
                    }
                    events.push(sse::output_item_done(output_index, item));
                }
                _ => {
                    let mut added = item.clone();
                    added["status"] = json!("in_progress");
                    events.push(sse::output_item_added(output_index, &added));
                    events.push(sse::output_item_done(output_index, item));
                }
            }
        }
    }

    match response.get("status").and_then(Value::as_str) {
        Some("incomplete") => events.push(sse::response_incomplete(response)),
        Some("failed") => events.push(sse::response_failed(response)),
        _ => events.push(sse::response_completed(response)),
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::request::GeminiThinking;
    use crate::proxy::providers::transform_codex_chat::build_codex_tool_context_from_request_with_web_search;
    use crate::proxy::providers::transform_gemini::{
        extract_gemini_tool_schema_hints, responses_to_gemini,
    };
    use futures::{stream, StreamExt};

    fn tool_context(body: &Value) -> CodexToolContext {
        build_codex_tool_context_from_request_with_web_search(
            body,
            true,
            Some("provider".to_string()),
        )
    }

    fn parse_events(output: &str) -> Vec<Value> {
        output
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str(data).ok())
            .collect()
    }

    fn output_items(events: &[Value], item_type: &str) -> Vec<Value> {
        events
            .iter()
            .filter(|event| event["type"] == "response.output_item.added")
            .filter_map(|event| event.get("item"))
            .filter(|item| item["type"] == item_type)
            .cloned()
            .collect()
    }

    fn collect_stream(
        chunks: Vec<String>,
        shadow_store: Option<Arc<GeminiShadowStore>>,
        provider_id: Option<String>,
        session_id: Option<String>,
        hints: Option<GeminiToolSchemaHints>,
        context: CodexToolContext,
    ) -> String {
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
        );
        let converted = create_responses_sse_stream_from_gemini(
            stream,
            shadow_store,
            provider_id,
            session_id,
            hints,
            context,
            "gemini-test".to_string(),
        );
        futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        })
    }

    fn collect_stream_output(chunks: Vec<&str>) -> String {
        collect_stream(
            chunks.into_iter().map(ToString::to_string).collect(),
            None,
            None,
            None,
            None,
            CodexToolContext::default(),
        )
    }

    fn collect_stream_output_with_shadow(
        chunks: Vec<&str>,
        store: Arc<GeminiShadowStore>,
        provider_id: &str,
        session_id: &str,
    ) -> String {
        collect_stream(
            chunks.into_iter().map(ToString::to_string).collect(),
            Some(store),
            Some(provider_id.to_string()),
            Some(session_id.to_string()),
            None,
            CodexToolContext::default(),
        )
    }

    async fn collect_chunked_output(payload: &[u8], chunk_size: usize) -> String {
        let chunks = payload
            .chunks(chunk_size)
            .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(chunk)))
            .collect::<Vec<_>>();
        create_responses_sse_stream_from_gemini(
            stream::iter(chunks),
            None,
            None,
            None,
            None,
            CodexToolContext::default(),
            "gemini-test".to_string(),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|chunk| String::from_utf8(chunk.unwrap().to_vec()).unwrap())
        .collect()
    }

    #[test]
    fn converts_cumulative_text_to_responses_deltas_once() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"totalTokenCount\":13}}\n\n",
            "data: {\"responseId\":\"resp_1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"totalTokenCount\":15}}\n\n",
        ]);
        let events = parse_events(&output);
        let deltas = events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<Vec<_>>();

        assert!(output.contains("event: response.created"));
        assert!(output.contains("event: response.in_progress"));
        assert_eq!(deltas, vec!["Hel", "lo"]);
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "response.completed")
                .count(),
            1
        );
        assert!(!output.contains("event: response.incomplete"));
        assert_eq!(
            events.last().unwrap()["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
    }

    #[test]
    fn converts_function_calls_to_responses_items_and_arguments() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}},\"thoughtSignature\":\"sig-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);
        let events = parse_events(&output);
        let calls = output_items(&events, "function_call");
        let argument_deltas = events
            .iter()
            .filter(|event| event["type"] == "response.function_call_arguments.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<Vec<_>>();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["name"], "get_weather");
        assert_eq!(calls[0]["call_id"], "call_1");
        assert_eq!(calls[0]["arguments"], "");
        assert_eq!(argument_deltas, vec![r#"{"city":"Tokyo"}"#]);
        assert!(output.contains("event: response.function_call_arguments.done"));
        assert_eq!(events.last().unwrap()["type"], "response.completed");
    }

    #[test]
    fn parses_crlf_delimited_cumulative_text_stream() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"resp_3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\r\n\r\n",
            "data: {\"responseId\":\"resp_3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"Hi there\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\r\n\r\n",
        ]);
        let events = parse_events(&output);
        let deltas = events
            .iter()
            .filter(|event| event["type"] == "response.output_text.delta")
            .filter_map(|event| event["delta"].as_str())
            .collect::<Vec<_>>();

        assert_eq!(deltas, vec!["Hi", " there"]);
        assert_eq!(events.last().unwrap()["type"], "response.completed");
    }

    #[test]
    fn preserves_utf8_when_json_payload_spans_transport_chunks() {
        let payload = json!({
            "responseId": "resp_utf8",
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{
                "finishReason": "STOP",
                "content": {"parts": [{ "text": "你好，Gemini" }]}
            }],
            "usageMetadata": {"promptTokenCount": 4, "totalTokenCount": 8}
        });
        let chunk = format!("data: {}\n\n", serde_json::to_string(&payload).unwrap());
        let split_at = chunk.find("你好").unwrap() + 1;
        let chunk_bytes = chunk.into_bytes();
        let source = futures::stream::iter([
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk_bytes[..split_at].to_vec())),
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk_bytes[split_at..].to_vec())),
        ]);
        let converted = create_responses_sse_stream_from_gemini(
            source,
            None,
            None,
            None,
            None,
            CodexToolContext::default(),
            "gemini-test".to_string(),
        );
        let output = futures::executor::block_on(async move {
            converted
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .map(|item| String::from_utf8(item.unwrap().to_vec()).unwrap())
                .collect::<Vec<_>>()
                .join("")
        });

        assert!(output.contains("你好，Gemini"));
        assert!(!output.contains('\u{fffd}'));
        assert!(output.contains("event: response.completed"));
    }

    #[test]
    fn stores_complete_assistant_shadow_and_replays_its_signature() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        collect_stream_output_with_shadow(
            vec![
                "data: {\"responseId\":\"resp_4\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
                "data: {\"responseId\":\"resp_4\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"text\":\"lo\"},{\"text\":\"\",\"thoughtSignature\":\"sig-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":8}}\n\n",
            ],
            store.clone(),
            "provider-a",
            "session-1",
        );

        let shadow = store
            .latest_assistant_content("provider-a", "session-1")
            .unwrap();
        assert_eq!(shadow["parts"][0]["text"], "Hello");
        assert_eq!(shadow["parts"][0]["thoughtSignature"], "sig-1");

        let input = json!({
            "model": "gemini-2.5-pro",
            "input": [
                {"role": "user", "content": "Hi"},
                {"role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]},
                {"role": "user", "content": "Continue"}
            ]
        });
        let context = tool_context(&input);
        let (second_turn, _) = responses_to_gemini(
            &input,
            Some(store.as_ref()),
            Some("provider-a"),
            Some("session-1"),
            &context,
            true,
            GeminiThinking::Auto,
        )
        .unwrap();

        assert_eq!(second_turn["contents"][1]["role"], "model");
        assert_eq!(second_turn["contents"][1]["parts"][0]["text"], "Hello");
        assert_eq!(
            second_turn["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-1"
        );
    }

    #[test]
    fn records_tool_shadow_before_yielding_tool_output_item() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        let source = stream::iter([Ok::<Bytes, std::io::Error>(Bytes::from_static(
            b"data: {\"responseId\":\"resp_tool_shadow\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"Bash\",\"args\":{\"command\":\"ls -R\"}},\"thoughtSignature\":\"sig-tool-1\"}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ))]);
        let mut converted = Box::pin(create_responses_sse_stream_from_gemini(
            source,
            Some(store.clone()),
            Some("provider-a".to_string()),
            Some("session-1".to_string()),
            None,
            CodexToolContext::default(),
            "gemini-test".to_string(),
        ));

        futures::executor::block_on(async {
            while let Some(item) = converted.next().await {
                let event = String::from_utf8(item.unwrap().to_vec()).unwrap();
                if event.contains(r#""type":"function_call""#) {
                    break;
                }
            }
        });

        let shadow = store
            .latest_assistant_content("provider-a", "session-1")
            .unwrap();
        assert_eq!(shadow["parts"][0]["functionCall"]["name"], "Bash");
        assert_eq!(shadow["parts"][0]["thoughtSignature"], "sig-tool-1");
    }

    #[test]
    fn repairs_tool_arguments_using_responses_schema_hints() {
        let request = json!({
            "tools": [{
                "type": "function",
                "name": "Bash",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "number"}
                    },
                    "required": ["command"]
                }
            }],
            "input": "run"
        });
        let context = tool_context(&request);
        let hints = extract_gemini_tool_schema_hints(&context);
        let output = collect_stream(vec![
            "data: {\"responseId\":\"resp_5\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"Bash\",\"args\":{\"args\":\"git status\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n"
                .to_string(),
        ], None, None, None, Some(hints), context);
        let events = parse_events(&output);
        let args = events
            .iter()
            .find(|event| event["type"] == "response.function_call_arguments.done")
            .unwrap()["arguments"]
            .as_str()
            .unwrap();
        assert_eq!(args, r#"{"command":"git status"}"#);
    }

    #[test]
    fn repairs_nested_skill_arguments_from_responses_schema_hints() {
        let request = json!({
            "tools": [{
                "type": "function",
                "name": "Skill",
                "parameters": {
                    "type": "object",
                    "properties": {"skill": {"type": "string"}, "args": {"type": "string"}},
                    "required": ["skill"]
                }
            }],
            "input": "run"
        });
        let context = tool_context(&request);
        let hints = extract_gemini_tool_schema_hints(&context);
        let payload = json!({
            "responseId": "resp_6",
            "modelVersion": "gemini-2.5-pro",
            "candidates": [{
                "finishReason": "STOP",
                "content": {"parts": [{
                    "functionCall": {
                        "id": "call_1",
                        "name": "Skill",
                        "args": {
                            "name": "git-commit",
                            "parameters": {"args": ["详细分析内容 编写提交信息 分多次提交代码"]}
                        }
                    }
                }]}
            }],
            "usageMetadata": {"promptTokenCount": 5, "totalTokenCount": 8}
        });
        let output = collect_stream(
            vec![format!(
                "data: {}\n\n",
                serde_json::to_string(&payload).unwrap()
            )],
            None,
            None,
            None,
            Some(hints),
            context,
        );
        let events = parse_events(&output);
        let response = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        let call = response["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        assert!(call["arguments"].as_str().unwrap().contains("git-commit"));
        assert!(call["arguments"]
            .as_str()
            .unwrap()
            .contains("详细分析内容 编写提交信息 分多次提交代码"));
        assert!(!call["arguments"].as_str().unwrap().contains("parameters"));
    }

    #[test]
    fn preserves_parallel_no_id_calls_with_distinct_synthesized_ids() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}},{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Osaka\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);
        let events = parse_events(&output);
        let calls = output_items(&events, "function_call");

        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0]["call_id"], calls[1]["call_id"]);
        assert!(calls.iter().all(|item| item["call_id"]
            .as_str()
            .unwrap()
            .starts_with("gemini_synth_")));
        let completed = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap();
        let arguments = completed["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call")
            .filter_map(|item| item["arguments"].as_str())
            .collect::<Vec<_>>();
        assert!(arguments.iter().any(|args| args.contains("Tokyo")));
        assert!(arguments.iter().any(|args| args.contains("Osaka")));
    }

    #[test]
    fn merges_cumulative_no_id_tool_call_snapshots() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
            "data: {\"responseId\":\"r2\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\",\"units\":\"c\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
        ]);
        let events = parse_events(&output);
        let calls = output_items(&events, "function_call");
        let completed = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap();

        assert_eq!(calls.len(), 1);
        assert!(completed["response"]["output"][0]["arguments"]
            .as_str()
            .unwrap()
            .contains("units"));
    }

    #[test]
    fn treats_empty_tool_call_ids_as_missing() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"r3\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}},{\"functionCall\":{\"id\":\"\",\"name\":\"get_weather\",\"args\":{\"city\":\"Osaka\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":5,\"totalTokenCount\":8}}\n\n",
        ]);
        let events = parse_events(&output);
        let calls = output_items(&events, "function_call");

        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|item| {
            item["call_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("gemini_synth_"))
        }));
    }

    #[test]
    fn upgrades_a_synthesized_id_when_a_later_snapshot_has_a_real_id() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"rupg\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
            "data: {\"responseId\":\"rupg\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"real_id_abc\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\",\"units\":\"c\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
        ]);
        let events = parse_events(&output);
        let calls = output_items(&events, "function_call");
        let response = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["call_id"], "real_id_abc");
        assert_eq!(response["response"]["output"][0]["call_id"], "real_id_abc");
        assert!(!response.to_string().contains("gemini_synth_"));
    }

    #[test]
    fn retains_tool_signature_when_a_later_snapshot_omits_it() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        collect_stream_output_with_shadow(
            vec![
                "data: {\"responseId\":\"rsig\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\"}},\"thoughtSignature\":\"sig-keep\"}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":6}}\n\n",
                "data: {\"responseId\":\"rsig\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"get_weather\",\"args\":{\"city\":\"Tokyo\",\"units\":\"c\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":4,\"totalTokenCount\":9}}\n\n",
            ],
            store.clone(),
            "provider-sig",
            "session-sig",
        );

        let shadow = store
            .latest_assistant_content("provider-sig", "session-sig")
            .expect("shadow turn must be recorded");
        assert_eq!(shadow["parts"][0]["functionCall"]["id"], "call_1");
        assert_eq!(shadow["parts"][0]["thoughtSignature"], "sig-keep");
    }

    #[tokio::test]
    async fn parses_terminal_sse_event_without_blank_line_at_eof() {
        let payload = br#"data: {"responseId":"eof-1","modelVersion":"gemini-2.5-pro","candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"complete"}]}}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":5}}"#;

        for chunk_size in [1, 7, 4096] {
            let output = collect_chunked_output(payload, chunk_size).await;
            let events = parse_events(&output);
            assert!(
                output.contains("\"text\":\"complete\""),
                "chunk_size={chunk_size}"
            );
            assert_eq!(
                events.last().unwrap()["type"],
                "response.completed",
                "chunk_size={chunk_size}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["type"] == "response.completed")
                    .count(),
                1,
                "chunk_size={chunk_size}"
            );
        }
    }

    #[test]
    fn max_tokens_does_not_commit_partial_tool_or_shadow_state() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        let output = collect_stream_output_with_shadow(
            vec![
                "data: {\"responseId\":\"incomplete-1\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"finishReason\":\"MAX_TOKENS\",\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_partial\",\"name\":\"lookup\",\"args\":{\"query\":\"unfinished\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":7}}\n\n",
            ],
            store.clone(),
            "provider-incomplete",
            "session-incomplete",
        );
        let events = parse_events(&output);
        let incomplete = events
            .iter()
            .find(|event| event["type"] == "response.incomplete")
            .unwrap();

        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert!(output_items(&events, "function_call").is_empty());
        assert!(store
            .get_session("provider-incomplete", "session-incomplete")
            .is_none());
    }

    #[test]
    fn safety_finish_reason_is_incomplete_and_does_not_commit_tools() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"safety-1\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"finishReason\":\"SAFETY\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{\"query\":\"restricted\"}}}]}}],\"usageMetadata\":{\"promptTokenCount\":3,\"totalTokenCount\":7}}\n\n",
        ]);
        let events = parse_events(&output);
        let incomplete = events
            .iter()
            .find(|event| event["type"] == "response.incomplete")
            .unwrap();

        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "content_filter"
        );
        assert!(output_items(&events, "function_call").is_empty());
    }

    #[test]
    fn maps_grounding_queries_to_responses_search_calls_once() {
        let context = tool_context(&json!({ "tools": [{ "type": "web_search" }] }));
        let output = collect_stream(
            vec![
                "data: {\"responseId\":\"search-1\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Finding.\"}]},\"groundingMetadata\":{\"webSearchQueries\":[\"weather\",\"weather\"]}}]}\n\n"
                    .to_string(),
                "data: {\"responseId\":\"search-1\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Finding the weather.\"}]},\"finishReason\":\"STOP\",\"groundingMetadata\":{\"webSearchQueries\":[\"weather\"]}}]}\n\n"
                    .to_string(),
            ],
            None,
            None,
            None,
            None,
            context,
        );
        let events = parse_events(&output);
        let search = output_items(&events, "web_search_call");
        assert_eq!(search.len(), 1);
        assert_eq!(search[0]["id"], "ws_search-1_search_0");
        assert_eq!(search[0]["action"]["query"], "weather");
        assert_eq!(events.last().unwrap()["type"], "response.completed");
    }

    #[test]
    fn maps_namespace_custom_and_tool_search_calls_to_responses_items() {
        let request = json!({
            "tools": [
                {
                    "type": "namespace",
                    "name": "repo",
                    "tools": [
                        {"type": "function", "name": "read", "parameters": {"type": "object"}},
                        {"type": "custom", "name": "shell"}
                    ]
                },
                {"type": "tool_search"}
            ],
            "input": "run"
        });
        let context = tool_context(&request);
        let output = collect_stream(
            vec![
                "data: {\"responseId\":\"calls-1\",\"modelVersion\":\"gemini-3-pro\",\"candidates\":[{\"finishReason\":\"STOP\",\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"repo__read\",\"args\":{\"path\":\"/tmp/a\"}}},{\"functionCall\":{\"name\":\"repo__shell\",\"args\":{\"input\":\"echo ok\"}}},{\"functionCall\":{\"name\":\"tool_search\",\"args\":{\"query\":\"filesystem\"}}}]}}]}\n\n"
                    .to_string(),
            ],
            None,
            None,
            None,
            None,
            context,
        );
        let events = parse_events(&output);
        let calls = events
            .iter()
            .find(|event| event["type"] == "response.completed")
            .unwrap()["response"]["output"]
            .as_array()
            .unwrap();
        assert_eq!(calls[0]["type"], "function_call");
        assert_eq!(calls[0]["name"], "read");
        assert_eq!(calls[0]["namespace"], "repo");
        assert_eq!(calls[1]["type"], "custom_tool_call");
        assert_eq!(calls[1]["name"], "shell");
        assert_eq!(calls[1]["namespace"], "repo");
        assert_eq!(calls[1]["input"], "echo ok");
        assert_eq!(calls[2]["type"], "tool_search_call");
        assert_eq!(calls[2]["arguments"]["query"], "filesystem");
        assert!(events
            .iter()
            .any(|event| event["type"] == "response.custom_tool_call_input.done"));
    }

    #[test]
    fn done_marker_without_finish_reason_remains_incomplete() {
        let output = collect_stream_output(vec![
            "data: {\"responseId\":\"done-only\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let events = parse_events(&output);
        let incomplete = events
            .iter()
            .find(|event| event["type"] == "response.incomplete")
            .unwrap();

        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert!(!output.contains("[DONE]"));
        assert!(!output.contains("event: response.completed"));
    }

    #[tokio::test]
    async fn stream_error_does_not_record_shadow_state() {
        let store = Arc::new(GeminiShadowStore::with_limits(8, 4));
        let source = stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::from_static(
                b"data: {\"responseId\":\"interrupted\",\"modelVersion\":\"gemini-2.5-pro\",\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"call_1\",\"name\":\"lookup\",\"args\":{}}}]}}]}\n\n",
            )),
            Err(std::io::Error::other("connection interrupted")),
        ]);
        let converted = create_responses_sse_stream_from_gemini(
            source,
            Some(store.clone()),
            Some("provider-interrupted".to_string()),
            Some("session-interrupted".to_string()),
            None,
            CodexToolContext::default(),
            "gemini-test".to_string(),
        );
        let chunks = converted.collect::<Vec<_>>().await;

        assert!(chunks.iter().any(Result::is_err));
        assert!(store
            .get_session("provider-interrupted", "session-interrupted")
            .is_none());
    }
}
