//! Direct conversion between OpenAI Responses and Gemini Native payloads.

use super::codex_compaction;
use super::gemini_schema::{build_gemini_function_declaration, normalize_gemini_function_schema};
use super::gemini_shadow::{GeminiAssistantTurn, GeminiShadowStore, GeminiToolCallMeta};
use super::transform_codex_chat::{
    response_tool_call_item_from_chat_name, response_tool_call_item_id_from_chat_name,
    responses_text_format, CodexToolContext,
};
use crate::compat::request::GeminiThinking;
use crate::proxy::error::ProxyError;
use crate::proxy::json_canonical::{canonical_json_string, canonicalize_tool_arguments};
use crate::proxy::providers::transform_responses::TOOL_RESULT_ERROR_MARKER;
use crate::proxy::tool_media::{
    strip_and_clamp_media_from_tool_value, ToolMediaScope, TOOL_RESULT_MEDIA_ATTACHED_MARKER,
};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeminiToolSchemaHint {
    expected_keys: Vec<String>,
    required_keys: Vec<String>,
}

pub type GeminiToolSchemaHints = HashMap<String, GeminiToolSchemaHint>;
pub(crate) const SYNTHESIZED_ID_PREFIX: &str = "gemini_synth_";
const GEMINI_SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

pub(crate) fn synthesize_tool_call_id() -> String {
    format!("{SYNTHESIZED_ID_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

pub(crate) fn is_synthesized_tool_call_id(id: &str) -> bool {
    id.starts_with(SYNTHESIZED_ID_PREFIX)
}

pub fn supports_gemini_thinking_level(model: &str) -> bool {
    let model = crate::proxy::gemini_url::normalize_gemini_model_id(model);
    let model = model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase();
    let Some(version) = model.strip_prefix("gemini-") else {
        return false;
    };
    version
        .split(|character: char| !character.is_ascii_digit())
        .next()
        .and_then(|major| major.parse::<u32>().ok())
        .is_some_and(|major| major >= 3)
}

fn is_gemini_3_series(model: &str) -> bool {
    let normalized = model.trim().to_ascii_lowercase();
    normalized.starts_with("gemini-3")
        || normalized
            .rsplit('/')
            .next()
            .is_some_and(|tail| tail.starts_with("gemini-3"))
}

pub(crate) fn responses_to_gemini(
    body: &Value,
    shadow_store: Option<&GeminiShadowStore>,
    provider_id: Option<&str>,
    session_id: Option<&str>,
    tool_context: &CodexToolContext,
    replay_thought_signatures: bool,
    thinking: GeminiThinking,
) -> Result<(Value, GeminiToolSchemaHints), ProxyError> {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    let shadow_turns = shadow_store
        .zip(provider_id)
        .zip(session_id)
        .and_then(|((store, provider_id), session_id)| store.get_session(provider_id, session_id))
        .map(|snapshot| snapshot.turns)
        .unwrap_or_default();

    let mut result = json!({});
    if let Some(system) = build_responses_system_instruction(body) {
        result["systemInstruction"] = system;
    }
    let requires_thought_signatures = supports_gemini_thinking_level(model);
    let supports_multimodal_function_response = is_gemini_3_series(model);
    let contents = convert_responses_input_to_contents(
        body,
        &shadow_turns,
        tool_context,
        replay_thought_signatures,
        requires_thought_signatures,
        supports_multimodal_function_response,
    )?;
    if !contents.is_empty() {
        result["contents"] = json!(contents
            .iter()
            .map(|message| json!({
                "role": if message.role == "model" { "model" } else { "user" },
                "parts": message.parts
            }))
            .collect::<Vec<_>>());
    }

    let mut generation_config =
        build_responses_generation_config(body, !tool_context.is_compaction_request())?;
    apply_thinking_level(
        &mut generation_config,
        model,
        body.pointer("/reasoning/effort").and_then(Value::as_str),
        thinking,
    )?;
    if !generation_config.is_empty() {
        result["generationConfig"] = Value::Object(generation_config);
    }

    let declarations = if tool_context.is_compaction_request() {
        Vec::new()
    } else {
        tool_context
            .chat_tools()
            .iter()
            .filter_map(|tool| {
                let function = tool.get("function")?;
                Some(build_gemini_function_declaration(
                    function.get("name").and_then(Value::as_str).unwrap_or(""),
                    function.get("description").and_then(Value::as_str),
                    function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                ))
            })
            .collect::<Vec<_>>()
    };
    let has_function_declarations = !declarations.is_empty();
    let has_web_search =
        !tool_context.is_compaction_request() && tool_context.hosted_web_search().is_some();
    let mut tools = Vec::new();
    if has_function_declarations {
        tools.push(json!({ "functionDeclarations": declarations }));
    }
    if has_web_search {
        tools.push(json!({ "googleSearch": {} }));
    }
    if !tools.is_empty() {
        result["tools"] = Value::Array(tools);
    }
    if has_function_declarations {
        if let Some(config) = map_responses_tool_choice(body.get("tool_choice"), tool_context)? {
            result["toolConfig"] = config;
        } else if body.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false)
            && body.get("tool_choice").is_none()
        {
            result["toolConfig"] = json!({ "functionCallingConfig": { "mode": "AUTO" } });
        }
    }

    Ok((result, extract_gemini_tool_schema_hints(tool_context)))
}

fn build_responses_system_instruction(body: &Value) -> Option<Value> {
    let mut texts = Vec::new();
    if let Some(instructions) = body.get("instructions").and_then(Value::as_str) {
        if !instructions.trim().is_empty() {
            texts.push(instructions.trim().to_string());
        }
    }
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            ) {
                texts.extend(responses_system_text(item));
            }
        }
    }
    (!texts.is_empty()).then(|| json!({ "parts": [{ "text": texts.join("\n\n") }] }))
}

fn responses_system_text(item: &Value) -> Vec<String> {
    let meaningful = |text: &str| !text.trim().is_empty();
    match item.get("content") {
        Some(Value::String(text)) if meaningful(text) => vec![text.trim().to_string()],
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                matches!(
                    part.get("type").and_then(Value::as_str),
                    Some("input_text" | "output_text" | "text")
                )
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
                .filter(|text| meaningful(text))
                .map(|text| text.trim().to_string())
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn build_responses_generation_config(
    body: &Value,
    include_text_format: bool,
) -> Result<Map<String, Value>, ProxyError> {
    let mut config = Map::new();
    if let Some(value) = body
        .get("max_output_tokens")
        .filter(|value| value.as_u64().is_some_and(|value| value > 0))
    {
        config.insert("maxOutputTokens".to_string(), value.clone());
    }
    for (source, target) in [("temperature", "temperature"), ("top_p", "topP")] {
        if let Some(value) = body.get(source) {
            config.insert(target.to_string(), value.clone());
        }
    }
    if include_text_format {
        if let Some(format) = responses_text_format(body)? {
            match format.get("type").and_then(Value::as_str) {
                Some("json_schema") => {
                    let schema = format
                        .get("schema")
                        .filter(|schema| schema.is_object())
                        .ok_or_else(|| {
                            ProxyError::InvalidRequest(
                                "Gemini Native structured output requires an object schema"
                                    .to_string(),
                            )
                        })?;
                    config.insert("responseMimeType".to_string(), json!("application/json"));
                    config.insert("responseJsonSchema".to_string(), schema.clone());
                }
                Some("json_object") => {
                    return Err(ProxyError::InvalidRequest(
                        "Gemini Native requires a JSON schema for structured output".to_string(),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(config)
}

fn apply_thinking_level(
    config: &mut Map<String, Value>,
    model: &str,
    effort: Option<&str>,
    thinking: GeminiThinking,
) -> Result<(), ProxyError> {
    if thinking != GeminiThinking::Auto || !supports_gemini_thinking_level(model) {
        return Ok(());
    }
    let Some(effort) = effort.map(str::trim).filter(|effort| !effort.is_empty()) else {
        return Ok(());
    };
    let level = match effort.to_ascii_lowercase().as_str() {
        "minimal" => "MINIMAL",
        "low" => "LOW",
        "medium" => "MEDIUM",
        "high" | "xhigh" | "max" | "ultra" => "HIGH",
        "none" | "off" | "disabled" => return Ok(()),
        _ => {
            return Err(ProxyError::InvalidRequest(format!(
                "Unsupported reasoning.effort for Gemini: {effort}"
            )))
        }
    };
    config.insert(
        "thinkingConfig".to_string(),
        json!({ "thinkingLevel": level }),
    );
    Ok(())
}

#[derive(Debug, Clone, Default)]
struct GeminiContentMessage {
    role: String,
    parts: Vec<Value>,
    function_calls: Vec<(String, String)>,
    function_responses: Vec<String>,
}

fn convert_responses_input_to_contents(
    body: &Value,
    shadow_turns: &[GeminiAssistantTurn],
    tool_context: &CodexToolContext,
    replay_thought_signatures: bool,
    requires_thought_signatures: bool,
    supports_multimodal_function_response: bool,
) -> Result<Vec<GeminiContentMessage>, ProxyError> {
    let mut messages = Vec::new();
    let input = match body.get("input") {
        Some(Value::Array(items)) => items.as_slice(),
        Some(Value::String(text)) if !text.trim().is_empty() => {
            let message = GeminiContentMessage {
                role: "user".to_string(),
                parts: vec![json!({ "text": text })],
                ..GeminiContentMessage::default()
            };
            messages.push(message);
            return finish_responses_input_messages(
                messages,
                shadow_turns,
                replay_thought_signatures,
                requires_thought_signatures,
            );
        }
        _ => &[],
    };

    let mut tool_name_by_id = build_tool_name_map_from_shadow_turns(shadow_turns);
    for item in input {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let flat_name = match item_type {
            "function_call" | "custom_tool_call" => tool_context.chat_name_for_response_function(
                item.get("name").and_then(Value::as_str).unwrap_or(""),
                item.get("namespace").and_then(Value::as_str),
            ),
            "tool_search_call" => "tool_search".to_string(),
            _ => continue,
        };
        let id = response_call_id(item);
        if !id.is_empty() && !flat_name.is_empty() {
            tool_name_by_id.entry(id.to_string()).or_insert(flat_name);
        }
    }

    for item in input {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        if item.get("status").and_then(Value::as_str) == Some("incomplete")
            && matches!(
                item_type,
                "function_call" | "custom_tool_call" | "tool_search_call" | "web_search_call"
            )
        {
            continue;
        }
        match item_type {
            "function_call" => {
                let id = response_call_id(item).to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let flat_name = tool_context.chat_name_for_response_function(
                    name,
                    item.get("namespace").and_then(Value::as_str),
                );
                let arguments = canonicalize_tool_arguments(item.get("arguments"));
                let mut args = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str::<Value>(&arguments).map_err(|error| {
                        ProxyError::InvalidRequest(format!(
                            "Invalid function_call arguments for '{name}': {error}"
                        ))
                    })?
                };
                if !args.is_object() {
                    return Err(ProxyError::InvalidRequest(format!(
                        "Function call arguments for '{name}' must be a JSON object"
                    )));
                }
                args =
                    crate::proxy::providers::transform_responses::sanitize_anthropic_tool_use_input(
                        name, args,
                    );
                let mut function_call = json!({ "name": flat_name, "args": args });
                if !id.is_empty() && !is_synthesized_tool_call_id(&id) {
                    function_call["id"] = json!(id);
                }
                append_function_call(&mut messages, &id, &flat_name, function_call);
            }
            "custom_tool_call" => {
                let id = response_call_id(item).to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                let name = tool_context.chat_name_for_response_function(
                    name,
                    item.get("namespace").and_then(Value::as_str),
                );
                let mut call = json!({
                    "name": name,
                    "args": { "input": item.get("input").cloned().unwrap_or_else(|| json!("")) }
                });
                if !id.is_empty() && !is_synthesized_tool_call_id(&id) {
                    call["id"] = json!(id);
                }
                append_function_call(&mut messages, &id, &name, call);
            }
            "tool_search_call" => {
                let id = response_call_id(item).to_string();
                let args = item
                    .get("arguments")
                    .filter(|value| value.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let mut call = json!({ "name": "tool_search", "args": args });
                if !id.is_empty() && !is_synthesized_tool_call_id(&id) {
                    call["id"] = json!(id);
                }
                append_function_call(&mut messages, &id, "tool_search", call);
            }
            "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
                let id = response_call_id(item).to_string();
                let Some(name) = tool_name_by_id.get(&id).cloned() else {
                    continue;
                };
                let (response, media_parts) = plan_responses_tool_result(item);
                let gemini_media_parts = media_parts
                    .iter()
                    .filter_map(gemini_part_from_chat_image)
                    .collect::<Vec<_>>();
                let mut function_response = json!({ "name": name, "response": response });
                if !id.is_empty() && !is_synthesized_tool_call_id(&id) {
                    function_response["id"] = json!(id);
                }
                if supports_multimodal_function_response && !gemini_media_parts.is_empty() {
                    function_response["parts"] = json!(gemini_media_parts);
                }
                let message = append_gemini_message(&mut messages, "user");
                message
                    .parts
                    .push(json!({ "functionResponse": function_response }));
                message.function_responses.push(id.clone());
                if !supports_multimodal_function_response {
                    for media in &gemini_media_parts {
                        message.parts.push(json!({ "text": format!("[cc-switch: media output of tool call {id}]") }));
                        message.parts.push(media.clone());
                    }
                }
            }
            "web_search_call" => { /* Gemini grounding history cannot be replayed. */ }
            "input_text" => {
                if let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    append_gemini_message(&mut messages, "user")
                        .parts
                        .push(json!({ "text": text }));
                }
            }
            "input_image" => {
                if let Some(part) = responses_image_part(item)? {
                    append_gemini_message(&mut messages, "user")
                        .parts
                        .push(part);
                }
            }
            "input_file" => {
                if let Some(part) = responses_file_part(item)? {
                    append_gemini_message(&mut messages, "user")
                        .parts
                        .push(part);
                }
            }
            "compaction_trigger" => append_gemini_message(&mut messages, "user")
                .parts
                .push(json!({ "text": codex_compaction::COMPACT_PROMPT })),
            "compaction" | "compaction_summary" | "context_compaction" => {
                if let Some(text) = codex_compaction::compaction_item_replay_text(item) {
                    append_gemini_message(&mut messages, "user")
                        .parts
                        .push(json!({ "text": text }));
                }
            }
            _ => {
                let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                if matches!(role, "system" | "developer") {
                    continue;
                }
                let role = if role == "assistant" { "model" } else { "user" };
                match item.get("content") {
                    Some(Value::String(text)) if !text.trim().is_empty() => {
                        append_gemini_message(&mut messages, role)
                            .parts
                            .push(json!({ "text": text }))
                    }
                    Some(Value::Array(parts)) => {
                        for part in parts {
                            if let Some(converted) = responses_content_part(part)? {
                                append_gemini_message(&mut messages, role)
                                    .parts
                                    .push(converted);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    finish_responses_input_messages(
        messages,
        shadow_turns,
        replay_thought_signatures,
        requires_thought_signatures,
    )
}

fn finish_responses_input_messages(
    mut messages: Vec<GeminiContentMessage>,
    shadow_turns: &[GeminiAssistantTurn],
    replay_thought_signatures: bool,
    requires_thought_signatures: bool,
) -> Result<Vec<GeminiContentMessage>, ProxyError> {
    drop_incomplete_tool_turns(&mut messages);
    drop_empty_gemini_messages(&mut messages);
    if !messages.is_empty() && messages[0].role != "user" {
        messages.insert(
            0,
            GeminiContentMessage {
                role: "user".to_string(),
                parts: vec![json!({ "text": "(continuing the conversation)" })],
                ..GeminiContentMessage::default()
            },
        );
    }
    drop_empty_gemini_messages(&mut messages);
    trim_trailing_assistant_text(&mut messages);
    drop_empty_gemini_messages(&mut messages);
    if messages.is_empty() {
        return Err(ProxyError::InvalidRequest(
            "cannot convert Codex request: empty messages".to_string(),
        ));
    }

    let total_assistant = messages
        .iter()
        .filter(|message| message.role == "model")
        .count();
    let effective = if shadow_turns.len() > total_assistant {
        &shadow_turns[shadow_turns.len() - total_assistant..]
    } else {
        shadow_turns
    };
    let start = total_assistant.saturating_sub(effective.len());
    let mut assistant_seen = 0usize;
    let mut used = HashSet::new();
    let mut names = build_tool_name_map_from_shadow_turns(shadow_turns);
    let mut signatures = if replay_thought_signatures {
        build_thought_signature_map_from_shadow_turns(shadow_turns)
    } else {
        HashMap::new()
    };
    for message in &mut messages {
        if message.role != "model" {
            continue;
        }
        let positional = assistant_seen
            .checked_sub(start)
            .filter(|index| *index < effective.len())
            .filter(|index| !used.contains(index));
        let matching =
            find_matching_shadow_turn(message, effective).filter(|index| !used.contains(index));
        assistant_seen += 1;
        if let Some(index) = matching.or(positional) {
            used.insert(index);
            let turn = &effective[index];
            merge_tool_names_from_shadow(turn, &mut names);
            if replay_thought_signatures {
                merge_thought_signatures_from_shadow(turn, &mut signatures);
            }
            if let Some(parts) = shadow_parts(&turn.assistant_content) {
                message.parts = if replay_thought_signatures {
                    parts
                } else {
                    strip_thought_signatures(parts)
                };
            }
        }
        for part in &mut message.parts {
            let Some(call) = part.get("functionCall") else {
                continue;
            };
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            if replay_thought_signatures {
                if let Some(signature) = signatures.get(id) {
                    part["thoughtSignature"] = json!(signature);
                }
            }
            if requires_thought_signatures && !has_thought_signature(part) {
                part["thoughtSignature"] = json!(GEMINI_SKIP_THOUGHT_SIGNATURE);
            }
        }
    }
    Ok(messages)
}

fn append_gemini_message<'a>(
    messages: &'a mut Vec<GeminiContentMessage>,
    role: &str,
) -> &'a mut GeminiContentMessage {
    if messages.last().is_none_or(|message| message.role != role) {
        messages.push(GeminiContentMessage {
            role: role.to_string(),
            ..GeminiContentMessage::default()
        });
    }
    messages.last_mut().expect("Gemini message appended")
}

fn append_function_call(
    messages: &mut Vec<GeminiContentMessage>,
    id: &str,
    name: &str,
    call: Value,
) {
    let message = append_gemini_message(messages, "model");
    message.parts.push(json!({ "functionCall": call }));
    message
        .function_calls
        .push((id.to_string(), name.to_string()));
}

fn response_call_id(item: &Value) -> &str {
    item.get("call_id")
        .or_else(|| item.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn responses_content_part(part: &Value) -> Result<Option<Value>, ProxyError> {
    match part.get("type").and_then(Value::as_str).unwrap_or("") {
        "input_text" | "output_text" | "text" => Ok(part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| json!({ "text": text }))),
        "refusal" => Ok(part
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(|text| json!({ "text": text }))),
        "input_image" => responses_image_part(part),
        "input_file" => responses_file_part(part),
        _ => Ok(None),
    }
}

fn responses_image_part(part: &Value) -> Result<Option<Value>, ProxyError> {
    let url = part
        .get("image_url")
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("url").and_then(Value::as_str))
        })
        .unwrap_or("");
    if url
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || url
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        return Err(ProxyError::TransformError(
            "Gemini Native only supports base64 image sources, got `url`".to_string(),
        ));
    }
    let Some(rest) = url.strip_prefix("data:") else {
        return Ok(None);
    };
    let Some((metadata, data)) = rest.split_once(',') else {
        return Ok(None);
    };
    let mime_type = metadata
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("image/png");
    Ok(Some(
        json!({ "inlineData": { "mimeType": mime_type, "data": data } }),
    ))
}

fn responses_file_part(part: &Value) -> Result<Option<Value>, ProxyError> {
    if part
        .get("file_url")
        .and_then(Value::as_str)
        .is_some_and(|url| {
            url.get(..7)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
                || url
                    .get(..8)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        })
    {
        return Err(ProxyError::TransformError(
            "Gemini Native only supports base64 document sources, got `url`".to_string(),
        ));
    }
    let Some(rest) = part
        .get("file_data")
        .and_then(Value::as_str)
        .and_then(|data| data.strip_prefix("data:"))
    else {
        return Ok(None);
    };
    let Some((metadata, data)) = rest.split_once(',') else {
        return Ok(None);
    };
    if data.is_empty() {
        return Ok(None);
    }
    let mime_type = metadata
        .split(';')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("application/pdf");
    Ok(Some(
        json!({ "inlineData": { "mimeType": mime_type, "data": data } }),
    ))
}

fn plan_responses_tool_result(item: &Value) -> (Value, Vec<Value>) {
    let mut cleaned = match item.get("output") {
        Some(Value::Array(parts)) => Value::Array(
            parts
                .iter()
                .filter_map(responses_tool_result_part)
                .collect(),
        ),
        Some(output) => output.clone(),
        None => json!(canonical_json_string(item)),
    };
    let replacement = json!({ "type": "text", "text": TOOL_RESULT_MEDIA_ATTACHED_MARKER });
    let mut media = Vec::new();
    let replaced = strip_and_clamp_media_from_tool_value(
        &mut cleaned,
        &mut media,
        ToolMediaScope::InlineImagesOnly,
        &replacement,
        TOOL_RESULT_MEDIA_ATTACHED_MARKER,
    );
    if replaced > 0 { /* extracted inline media is reattached in the native request */ }
    let content = match &cleaned {
        Value::String(text) => json!(text),
        Value::Array(parts) => {
            let texts = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>();
            if texts.is_empty() {
                Value::Array(parts.clone())
            } else {
                json!(texts.join("\n"))
            }
        }
        value => json!(canonical_json_string(value)),
    };
    (json!({ "content": content }), media)
}

fn responses_tool_result_part(part: &Value) -> Option<Value> {
    let item_type = part.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "input_text" | "output_text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            (text != TOOL_RESULT_ERROR_MARKER).then(|| json!({ "type": "text", "text": text }))
        }
        "input_image" => Some(
            responses_tool_result_image(part)
                .unwrap_or_else(|| json!({ "type": "text", "text": canonical_json_string(part) })),
        ),
        "input_file" => Some(
            responses_tool_result_file(part)
                .unwrap_or_else(|| json!({ "type": "text", "text": canonical_json_string(part) })),
        ),
        _ => Some(json!({
            "type": "text",
            "text": canonical_json_string(part)
        })),
    }
}

fn responses_tool_result_image(part: &Value) -> Option<Value> {
    let url = part.get("image_url").and_then(|value| {
        value
            .as_str()
            .or_else(|| value.get("url").and_then(Value::as_str))
    })?;
    if url
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        let rest = &url[5..];
        let (metadata, data) = rest.split_once(',')?;
        return Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": metadata.split(';').next().unwrap_or("image/png"),
                "data": data
            }
        }));
    }
    if url
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
        || url
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
    {
        return Some(json!({
            "type": "image",
            "source": { "type": "url", "url": url }
        }));
    }
    None
}

fn responses_tool_result_file(part: &Value) -> Option<Value> {
    let filename = part
        .get("filename")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let mut document = if let Some(url) = part
        .get("file_url")
        .and_then(Value::as_str)
        .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
    {
        json!({ "type": "document", "source": { "type": "url", "url": url } })
    } else {
        let rest = part
            .get("file_data")
            .and_then(Value::as_str)?
            .strip_prefix("data:")?;
        let (metadata, data) = rest.split_once(',')?;
        if data.is_empty() {
            return None;
        }
        json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": metadata.split(';').next().unwrap_or("application/pdf"),
                "data": data
            }
        })
    };
    if let Some(filename) = filename {
        document["title"] = json!(filename);
    }
    Some(document)
}

fn gemini_part_from_chat_image(part: &Value) -> Option<Value> {
    let url = part.pointer("/image_url/url").and_then(Value::as_str)?;
    let (metadata, data) = url.strip_prefix("data:")?.split_once(',')?;
    if data.is_empty() || !metadata.to_ascii_lowercase().contains(";base64") {
        return None;
    }
    Some(
        json!({ "inlineData": { "mimeType": metadata.split(';').next().unwrap_or("image/png"), "data": data } }),
    )
}

fn drop_incomplete_tool_turns(messages: &mut Vec<GeminiContentMessage>) {
    let original = std::mem::take(messages);
    let mut output = Vec::new();
    let mut index = 0;
    while index < original.len() {
        let message = &original[index];
        if message.role == "model" && !message.function_calls.is_empty() {
            let paired = original.get(index + 1).filter(|next| next.role == "user");
            let ids = message
                .function_calls
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<HashSet<_>>();
            let result_ids = paired
                .map(|next| {
                    next.function_responses
                        .iter()
                        .map(String::as_str)
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();
            let count_ids = message
                .function_calls
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>();
            let result_values = paired
                .map(|next| {
                    next.function_responses
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let complete = count_ids.iter().all(|id| !id.is_empty())
                && result_values.iter().all(|id| !id.is_empty())
                && ids.len() == count_ids.len()
                && result_ids.len() == result_values.len()
                && ids == result_ids;
            if complete {
                output.push(message.clone());
                output.push(paired.expect("complete pair has a user message").clone());
            } else if let Some(user) = paired {
                let mut user = user.clone();
                user.parts
                    .retain(|part| part.get("functionResponse").is_none());
                user.function_responses.clear();
                if !user.parts.is_empty() {
                    output.push(user);
                }
            }
            index += if paired.is_some() { 2 } else { 1 };
            continue;
        }
        let mut message = message.clone();
        if message.role == "user" {
            message
                .parts
                .retain(|part| part.get("functionResponse").is_none());
            message.function_responses.clear();
        }
        if !message.parts.is_empty() {
            output.push(message);
        }
        index += 1;
    }
    *messages = output;
}

fn trim_trailing_assistant_text(messages: &mut [GeminiContentMessage]) {
    let Some(last) = messages
        .last_mut()
        .filter(|message| message.role == "model")
    else {
        return;
    };
    let Some(part) = last.parts.last_mut() else {
        return;
    };
    let Some(text) = part.get("text").and_then(Value::as_str) else {
        return;
    };
    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        last.parts.pop();
    } else if trimmed.len() != text.len() {
        part["text"] = json!(trimmed);
    }
}

fn map_responses_tool_choice(
    choice: Option<&Value>,
    tool_context: &CodexToolContext,
) -> Result<Option<Value>, ProxyError> {
    let Some(choice) = choice else {
        return Ok(None);
    };
    let config = match choice {
        Value::String(choice) => match choice.as_str() {
            "auto" => json!({ "mode": "AUTO" }),
            "none" => json!({ "mode": "NONE" }),
            "required" => json!({ "mode": "ANY" }),
            _ => json!({ "mode": "AUTO" }),
        },
        Value::Object(choice) => match choice.get("type").and_then(Value::as_str) {
            Some("auto") => json!({ "mode": "AUTO" }),
            Some("none") => json!({ "mode": "NONE" }),
            Some("function" | "custom" | "tool_search") => {
                let name = if choice.get("type").and_then(Value::as_str) == Some("tool_search") {
                    "tool_search".to_string()
                } else {
                    tool_context.chat_name_for_response_function(
                        choice.get("name").and_then(Value::as_str).unwrap_or(""),
                        choice.get("namespace").and_then(Value::as_str),
                    )
                };
                json!({ "mode": "ANY", "allowedFunctionNames": [name] })
            }
            _ => json!({ "mode": "AUTO" }),
        },
        _ => return Ok(None),
    };
    Ok(Some(json!({ "functionCallingConfig": config })))
}

pub(crate) fn extract_gemini_tool_schema_hints(
    tool_context: &CodexToolContext,
) -> GeminiToolSchemaHints {
    if tool_context.is_compaction_request() {
        return GeminiToolSchemaHints::default();
    }
    tool_context
        .chat_tools()
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            let schema = normalize_gemini_function_schema(
                function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            );
            let properties = schema.get("properties").and_then(Value::as_object)?;
            if properties.is_empty() {
                return None;
            }
            let expected_keys = properties.keys().cloned().collect();
            let required_keys = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
            Some((
                name.to_string(),
                GeminiToolSchemaHint {
                    expected_keys,
                    required_keys,
                },
            ))
        })
        .collect()
}

pub(crate) fn rectify_tool_call_parts(parts: &mut [Value], hints: Option<&GeminiToolSchemaHints>) {
    for part in parts {
        let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) else {
            continue;
        };
        let Some(name) = call
            .get("name")
            .and_then(Value::as_str)
            .map(ToString::to_string)
        else {
            continue;
        };
        let Some(args) = call.get_mut("args") else {
            continue;
        };
        if rectify_tool_call_args(&name, args, hints) {
            log::info!("[Codex/Gemini] Rectified tool args for {name}");
        }
    }
}

pub(crate) fn rectify_tool_call_args(
    name: &str,
    args: &mut Value,
    hints: Option<&GeminiToolSchemaHints>,
) -> bool {
    let Some(hint) = hints.and_then(|hints| hints.get(name)) else {
        return false;
    };
    let Some(args) = args.as_object_mut() else {
        return false;
    };
    if args.is_empty() || hint.expected_keys.is_empty() {
        return false;
    }
    let mut changed = false;
    if hint.expected_keys.iter().any(|key| key == "skill") && !args.contains_key("skill") {
        if let Some(value) = args.remove("name") {
            args.insert("skill".into(), value);
            changed = true;
        }
    }
    if !hint.expected_keys.iter().any(|key| key == "parameters") {
        let lifted = args
            .get("parameters")
            .and_then(Value::as_object)
            .map(|parameters| {
                hint.expected_keys
                    .iter()
                    .filter(|key| !args.contains_key(key.as_str()))
                    .filter_map(|key| {
                        parameters.get(key).map(|value| {
                            (
                                key.clone(),
                                if let Value::Array(values) = value {
                                    if values.len() == 1 {
                                        values[0].clone()
                                    } else {
                                        value.clone()
                                    }
                                } else {
                                    value.clone()
                                },
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !lifted.is_empty() {
            for (key, value) in lifted {
                args.insert(key, value);
            }
            args.remove("parameters");
            changed = true;
        }
    }
    if hint
        .required_keys
        .iter()
        .all(|key| args.contains_key(key.as_str()))
    {
        return changed;
    }
    let expected = hint
        .expected_keys
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let unexpected = args
        .keys()
        .filter(|key| !expected.contains(key.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unexpected.len() != 1 {
        return changed;
    }
    let target = hint
        .required_keys
        .iter()
        .find(|key| !args.contains_key(key.as_str()))
        .cloned()
        .or_else(|| {
            (hint.expected_keys.len() == 1 && args.len() == 1)
                .then(|| hint.expected_keys[0].clone())
        });
    let Some(target) = target.filter(|target| !args.contains_key(target)) else {
        return changed;
    };
    if let Some(value) = args.remove(&unexpected[0]) {
        args.insert(target, value);
        true
    } else {
        changed
    }
}

fn has_thought_signature(part: &Value) -> bool {
    [
        part.get("thoughtSignature"),
        part.get("thought_signature"),
        part.pointer("/functionCall/thoughtSignature"),
        part.pointer("/functionCall/thought_signature"),
    ]
    .into_iter()
    .flatten()
    .any(|value| {
        value
            .as_str()
            .is_some_and(|signature| !signature.is_empty())
    })
}

fn strip_thought_signatures(mut parts: Vec<Value>) -> Vec<Value> {
    for part in &mut parts {
        if let Some(object) = part.as_object_mut() {
            object.remove("thoughtSignature");
            object.remove("thought_signature");
        }
        if let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) {
            call.remove("thoughtSignature");
            call.remove("thought_signature");
        }
    }
    parts
}

fn shadow_parts(content: &Value) -> Option<Vec<Value>> {
    let mut parts = content
        .get("parts")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| content.as_array().cloned())?;
    for part in &mut parts {
        let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) else {
            continue;
        };
        if call
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.is_empty() || is_synthesized_tool_call_id(id))
        {
            call.remove("id");
        }
    }
    Some(parts)
}

fn find_matching_shadow_turn(
    message: &GeminiContentMessage,
    turns: &[GeminiAssistantTurn],
) -> Option<usize> {
    let ids = message
        .function_calls
        .iter()
        .filter(|(id, _)| !id.is_empty())
        .map(|(id, _)| id.as_str())
        .collect::<HashSet<_>>();
    let names = message
        .function_calls
        .iter()
        .filter(|(_, name)| !name.is_empty())
        .map(|(_, name)| name.as_str())
        .collect::<HashSet<_>>();
    if !ids.is_empty() {
        if let Some(index) = turns.iter().position(|turn| {
            turn.tool_calls
                .iter()
                .any(|call| call.id.as_deref().is_some_and(|id| ids.contains(id)))
        }) {
            return Some(index);
        }
    }
    turns.iter().enumerate().find_map(|(index, turn)| {
        turn.tool_calls
            .iter()
            .any(|call| names.contains(call.name.as_str()))
            .then_some(index)
    })
}

fn build_tool_name_map_from_shadow_turns(turns: &[GeminiAssistantTurn]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for turn in turns {
        merge_tool_names_from_shadow(turn, &mut names);
    }
    names
}

fn drop_empty_gemini_messages(messages: &mut Vec<GeminiContentMessage>) {
    messages.retain(|message| !message.parts.is_empty());
}

fn merge_tool_names_from_shadow(turn: &GeminiAssistantTurn, names: &mut HashMap<String, String>) {
    for call in &turn.tool_calls {
        if let Some(id) = &call.id {
            names.insert(id.clone(), call.name.clone());
        }
    }
    if let Some(parts) = shadow_parts(&turn.assistant_content) {
        for part in parts {
            if let Some(call) = part.get("functionCall") {
                if let (Some(id), Some(name)) = (
                    call.get("id").and_then(Value::as_str),
                    call.get("name").and_then(Value::as_str),
                ) {
                    if !id.is_empty() && !name.is_empty() {
                        names.insert(id.into(), name.into());
                    }
                }
            }
        }
    }
}

fn build_thought_signature_map_from_shadow_turns(
    turns: &[GeminiAssistantTurn],
) -> HashMap<String, String> {
    let mut signatures = HashMap::new();
    for turn in turns {
        for call in &turn.tool_calls {
            if let (Some(id), Some(sig)) = (&call.id, &call.thought_signature) {
                signatures.insert(id.clone(), sig.clone());
            }
        }
    }
    signatures
}

fn merge_thought_signatures_from_shadow(
    turn: &GeminiAssistantTurn,
    signatures: &mut HashMap<String, String>,
) {
    for call in &turn.tool_calls {
        if let (Some(id), Some(sig)) = (&call.id, &call.thought_signature) {
            signatures.insert(id.clone(), sig.clone());
        }
    }
}

fn extract_tool_call_meta(parts: &[Value]) -> Vec<GeminiToolCallMeta> {
    parts
        .iter()
        .filter_map(|part| {
            let call = part.get("functionCall")?;
            Some(GeminiToolCallMeta::new(
                Some(
                    call.get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(ToString::to_string)
                        .unwrap_or_else(synthesize_tool_call_id),
                ),
                call.get("name").and_then(Value::as_str).unwrap_or(""),
                call.get("args").cloned().unwrap_or_else(|| json!({})),
                part.get("thoughtSignature")
                    .or_else(|| part.get("thought_signature"))
                    .and_then(Value::as_str),
            ))
        })
        .collect()
}

pub(crate) fn response_id(id: Option<&str>) -> String {
    match id.filter(|id| !id.is_empty()) {
        Some(id) if id.starts_with("resp_") => id.to_string(),
        Some(id) => format!("resp_{id}"),
        None => "resp_ccswitch".into(),
    }
}

pub(crate) fn build_responses_usage(usage: Option<&Value>) -> Value {
    let Some(usage) = usage else {
        return json!({ "input_tokens": 0, "output_tokens": 0, "total_tokens": 0, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens_details": { "reasoning_tokens": 0 } });
    };
    let input = usage
        .get("promptTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage.get("totalTokenCount").and_then(Value::as_u64);
    let output = usage
        .get("candidatesTokenCount")
        .and_then(Value::as_u64)
        .or_else(|| total.map(|total| total.saturating_sub(input)))
        .unwrap_or(0);
    let cached = usage
        .get("cachedContentTokenCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({ "input_tokens": input, "output_tokens": output, "total_tokens": total.unwrap_or_else(|| input.saturating_add(output)), "input_tokens_details": { "cached_tokens": cached }, "output_tokens_details": { "reasoning_tokens": 0 } })
}

pub(crate) fn extract_gemini_web_search_queries(candidate: &Value) -> Vec<String> {
    candidate
        .pointer("/groundingMetadata/webSearchQueries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub(crate) fn gemini_web_search_call_id(response_id: Option<&str>, index: usize) -> String {
    match response_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => format!("{id}_search_{index}"),
        None => format!("gemini_search_{}_{index}", uuid::Uuid::new_v4().simple()),
    }
}

pub(crate) fn map_gemini_finish_reason(
    reason: Option<&str>,
) -> (&'static str, Option<&'static str>) {
    match reason {
        Some("MAX_TOKENS") => ("incomplete", Some("max_output_tokens")),
        Some("SAFETY" | "RECITATION" | "SPII" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            ("incomplete", Some("content_filter"))
        }
        _ => ("completed", None),
    }
}

fn gemini_candidate_parts(
    candidate: &Value,
    hints: Option<&GeminiToolSchemaHints>,
    synthesize_ids: bool,
) -> Vec<Value> {
    let mut parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rectify_tool_call_parts(&mut parts, hints);
    if synthesize_ids {
        for part in &mut parts {
            if let Some(call) = part.get_mut("functionCall").and_then(Value::as_object_mut) {
                if call
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    call.insert("id".into(), json!(synthesize_tool_call_id()));
                }
            }
        }
    }
    parts
}

fn gemini_candidate_to_responses_output(
    body: &Value,
    candidate: &Value,
    parts: &[Value],
    context: &CodexToolContext,
    allow_calls: bool,
) -> Vec<Value> {
    let id = response_id(body.get("responseId").and_then(Value::as_str));
    let mut output = Vec::new();
    if context.hosted_web_search().is_some() {
        for (index, query) in extract_gemini_web_search_queries(candidate)
            .into_iter()
            .enumerate()
        {
            let call =
                gemini_web_search_call_id(body.get("responseId").and_then(Value::as_str), index);
            output.push(gemini_web_search_call_item(
                &format!("ws_{call}"),
                "completed",
                &query,
            ));
        }
    }
    let mut text_parts = Vec::new();
    let flush_text = |output: &mut Vec<Value>, text_parts: &mut Vec<Value>| {
        if !text_parts.is_empty() {
            let item_index = output.len();
            output.push(json!({
                "id": format!("{id}_msg_{item_index}"),
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": std::mem::take(text_parts)
            }));
        }
    };
    for part in parts {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                text_parts.push(json!({ "type": "output_text", "text": text, "annotations": [] }));
            }
            continue;
        }
        if !allow_calls {
            continue;
        }
        let Some(call) = part.get("functionCall") else {
            continue;
        };
        flush_text(&mut output, &mut text_parts);
        let call_id = call.get("id").and_then(Value::as_str).unwrap_or("");
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        let args = canonical_json_string(&call.get("args").cloned().unwrap_or_else(|| json!({})));
        let item_id = response_tool_call_item_id_from_chat_name(call_id, name, context);
        output.push(response_tool_call_item_from_chat_name(
            &item_id,
            "completed",
            call_id,
            name,
            &args,
            None,
            context,
        ));
    }
    flush_text(&mut output, &mut text_parts);
    output
}

pub(crate) fn gemini_web_search_call_item(id: &str, status: &str, query: &str) -> Value {
    json!({
        "id": id,
        "type": "web_search_call",
        "status": status,
        "action": { "type": "search", "query": query }
    })
}

pub(crate) fn gemini_to_responses(
    body: Value,
    shadow_store: Option<&GeminiShadowStore>,
    provider_id: Option<&str>,
    session_id: Option<&str>,
    tool_context: &CodexToolContext,
    hints: Option<&GeminiToolSchemaHints>,
) -> Result<Value, ProxyError> {
    let usage = build_responses_usage(body.get("usageMetadata"));
    let model = body
        .get("modelVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let blocked = body
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str);
    let candidate = body
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    let (status, reason) = if blocked.is_some() {
        ("incomplete", Some("content_filter"))
    } else {
        map_gemini_finish_reason(
            candidate
                .and_then(|candidate| candidate.get("finishReason"))
                .and_then(Value::as_str),
        )
    };
    let candidate_parts =
        candidate.map(|candidate| gemini_candidate_parts(candidate, hints, status == "completed"));
    let id = response_id(body.get("responseId").and_then(Value::as_str));
    let mut output = if let Some(blocked) = blocked {
        vec![
            json!({ "id": format!("{id}_msg_0"), "type": "message", "status": "completed", "role": "assistant", "content": [{ "type": "output_text", "text": format!("Request blocked by Gemini safety filters: {blocked}"), "annotations": [] }] }),
        ]
    } else {
        let candidate = candidate
            .ok_or_else(|| ProxyError::TransformError("No candidates in Gemini response".into()))?;
        gemini_candidate_to_responses_output(
            &body,
            candidate,
            candidate_parts.as_deref().unwrap_or_default(),
            tool_context,
            status == "completed",
        )
    };
    if status != "completed" {
        output.retain(|item| item.get("type").and_then(Value::as_str) != Some("function_call"));
    }
    if let (Some(store), Some(provider_id), Some(session_id), Some(candidate)) =
        (shadow_store, provider_id, session_id, candidate)
    {
        if status == "completed"
            && candidate.get("finishReason").and_then(Value::as_str) == Some("STOP")
        {
            let parts = candidate_parts.as_deref().unwrap_or_default();
            store.record_assistant_turn(
                provider_id,
                session_id,
                json!({ "parts": parts }),
                extract_tool_call_meta(parts),
            );
        }
    }
    let mut response = json!({ "id": id, "object": "response", "created_at": 0, "status": status, "model": model, "output": output, "usage": usage });
    if let Some(reason) = reason {
        response["incomplete_details"] = json!({ "reason": reason });
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(body: Value) -> CodexToolContext {
        super::super::transform_codex_chat::build_codex_tool_context_from_request_with_web_search(
            &body,
            true,
            Some("provider".into()),
        )
    }

    #[test]
    fn direct_responses_request_maps_system_media_tools_and_generation_options() {
        let input = json!({
            "model": "gemini-3-pro", "instructions": "Rules", "temperature": 0.2, "top_p": 0.7,
            "max_output_tokens": 512, "reasoning": {"effort":"high"},
            "text": {"format": {"type":"json_schema", "schema":{"type":"object"}}},
            "tools": [{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}],
            "tool_choice": {"type":"function","name":"lookup"},
            "input": [
                {"role":"developer","content":[{"type":"input_text","text":"Be brief."}]},
                {"role":"user","content":[{"type":"input_text","text":"Weather?"},{"type":"input_image","image_url":"data:image/png;base64,abc"}]}
            ]
        });
        let tools = context(input.clone());
        let (result, _) =
            responses_to_gemini(&input, None, None, None, &tools, true, GeminiThinking::Auto)
                .unwrap();
        assert_eq!(
            result["systemInstruction"]["parts"][0]["text"],
            "Rules\n\nBe brief."
        );
        assert_eq!(result["contents"][0]["role"], "user");
        assert_eq!(
            result["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(result["generationConfig"]["maxOutputTokens"], 512);
        assert_eq!(
            result["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert_eq!(
            result["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"],
            json!(["lookup"])
        );
    }

    #[test]
    fn direct_response_maps_usage_incomplete_and_grounding_without_partial_tools() {
        let tools = context(
            json!({"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{"city":{"type":"string"}}}},{"type":"web_search"}]}),
        );
        let response = gemini_to_responses(json!({
            "responseId":"gemini-1","modelVersion":"gemini-3-pro",
            "candidates":[{"content":{"parts":[{"text":"Searching."},{"functionCall":{"name":"lookup","args":{"city":"Tokyo"}}}]},"finishReason":"MAX_TOKENS","groundingMetadata":{"webSearchQueries":["weather"]}}],
            "usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":2,"candidatesTokenCount":5,"totalTokenCount":15}
        }), None, None, None, &tools, None).unwrap();
        assert_eq!(response["status"], "incomplete");
        assert_eq!(
            response["incomplete_details"]["reason"],
            "max_output_tokens"
        );
        assert_eq!(response["output"][0]["type"], "web_search_call");
        assert_eq!(response["output"][1]["content"][0]["text"], "Searching.");
        assert!(!response.to_string().contains("function_call"));
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(
            response["usage"]["input_tokens_details"]["cached_tokens"],
            2
        );
        assert_eq!(response["usage"]["output_tokens"], 5);
    }

    #[test]
    fn direct_response_and_shadow_share_synthesized_tool_call_id() {
        let tools = context(json!({
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }]
        }));
        let shadow = GeminiShadowStore::default();
        let response = gemini_to_responses(
            json!({
                "responseId": "gemini-id-parity",
                "modelVersion": "gemini-3-pro",
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"name": "lookup", "args": {"city": "Tokyo"}},
                        "thoughtSignature": "sig-id-parity"
                    }]},
                    "finishReason": "STOP"
                }]
            }),
            Some(&shadow),
            Some("provider"),
            Some("session"),
            &tools,
            None,
        )
        .unwrap();
        let call_id = response["output"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap()["call_id"]
            .as_str()
            .unwrap();
        let stored = shadow
            .get_session("provider", "session")
            .unwrap()
            .turns
            .pop()
            .unwrap();

        assert_eq!(
            stored.assistant_content["parts"][0]["functionCall"]["id"],
            call_id
        );
        assert_eq!(stored.tool_calls[0].id.as_deref(), Some(call_id));
    }

    #[test]
    fn direct_request_preserves_tool_result_error_media_and_unknown_part_handling() {
        let input = json!({
            "model": "gemini-3-pro",
            "tools": [{
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object", "properties": {}}
            }],
            "input": [
                {"type": "function_call", "call_id": "call-1", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call-1", "output": [
                    {"type": "input_text", "text": TOOL_RESULT_ERROR_MARKER},
                    {"type": "output_text", "text": "Image attached."},
                    {"type": "input_image", "image_url": "data:image/png;base64,aGVsbG8="},
                    {"type": "future_part", "payload": {"x": 1}}
                ]}
            ]
        });
        let tools = context(input.clone());
        let (request, _) =
            responses_to_gemini(&input, None, None, None, &tools, true, GeminiThinking::Auto)
                .unwrap();
        let function_response = request["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|content| content["parts"].as_array().into_iter().flatten())
            .find_map(|part| part.get("functionResponse"))
            .unwrap();
        let expected_unknown_part = canonical_json_string(&json!({
            "type": "future_part",
            "payload": {"x": 1}
        }));
        let response_content = function_response["response"]["content"].as_str().unwrap();

        assert_eq!(
            response_content,
            format!(
                "Image attached.\n{TOOL_RESULT_MEDIA_ATTACHED_MARKER}\n{expected_unknown_part}"
            )
        );
        assert!(!response_content.contains(TOOL_RESULT_ERROR_MARKER));
        assert_eq!(
            function_response["parts"][0]["inlineData"]["mimeType"],
            "image/png"
        );
        assert_eq!(
            function_response["parts"][0]["inlineData"]["data"],
            "aGVsbG8="
        );
    }

    #[test]
    fn direct_request_uses_gemini_3_missing_signature_fallback() {
        let make_request = |model: &str| {
            let input = json!({
                "model": model,
                "tools": [{
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object", "properties": {}}
                }],
                "input": [
                    {"role": "user", "content": "Look it up."},
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "done"}
                ]
            });
            let tools = context(input.clone());
            responses_to_gemini(&input, None, None, None, &tools, true, GeminiThinking::Auto)
                .unwrap()
                .0
        };

        let gemini_3 = make_request("gemini-3-pro");
        assert_eq!(
            gemini_3["contents"][1]["parts"][0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE
        );

        let gemini_2 = make_request("gemini-2.5-pro");
        assert!(gemini_2["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }
}
