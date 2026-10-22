// Derived from cc-switch, Copyright (c) 2025 Jason Young. MIT License.
// See LICENSE-cc-switch in this directory.
//! Codex compaction helpers shared by the Chat and Anthropic conversion bridges.
//!
//! Compaction triggers request a textual summary and return it inside a
//! `ccswitch-compaction-v1:` base64 envelope. These bridges restore that summary
//! when replaying history and explicitly mark foreign, unreadable envelopes.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};

pub(crate) const CCSWITCH_COMPACTION_PREFIX: &str = "ccswitch-compaction-v1:";

/// 与 codex-rs `prompts/templates/compact/prompt.md` 一致（Codex 本地压缩用的提示词）。
pub(crate) const COMPACT_PROMPT: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

Include:
- Current progress and key decisions made
- Important context, constraints, or user preferences
- What remains to be done (clear next steps)
- Any critical data, examples, or references needed to continue

Be concise, structured, and focused on helping the next LLM seamlessly continue the work.
";

/// 与 codex-rs `prompts/templates/compact/summary_prefix.md` 一致；Codex 本地压缩把摘要
/// 写成 `{SUMMARY_PREFIX}\n{summary}` 的用户消息，这里保持同一形态。
pub(crate) const SUMMARY_PREFIX: &str = "Another language model started to solve this problem and produced a summary of its thinking process. You also have access to the state of the tools that were used by that language model. Use this to build on the work that has already been done and avoid duplicating work. Here is the summary produced by the other language model, use the information in this summary to assist with your own analysis:";

/// 历史里的压缩条目是别家的密文、当前模型读不了时，用这句话占位。
pub(crate) const OPAQUE_COMPACTION_NOTE: &str =
    "[earlier conversation was compacted; the summary is stored in a format this model cannot read]";

pub(crate) fn is_compaction_trigger(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("compaction_trigger")
}

/// 这个 Responses 请求是不是一次 V2 压缩。
pub(crate) fn is_compaction_request(body: &Value) -> bool {
    body.get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(is_compaction_trigger))
}

pub(crate) fn encode_compaction_summary(summary: &str) -> String {
    format!(
        "{CCSWITCH_COMPACTION_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(summary.as_bytes())
    )
}

/// 解开 CC Switch 包装的摘要；别家密文或损坏的内容返回 None。
pub(crate) fn decode_compaction_summary(encrypted_content: &str) -> Option<String> {
    let encoded = encrypted_content.strip_prefix(CCSWITCH_COMPACTION_PREFIX)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    String::from_utf8(bytes).ok()
}

fn compaction_encrypted_content(item: &Value) -> Option<&str> {
    item.get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

/// 压缩条目回放给第三方模型时的文字。没有载荷的 `context_compaction` 只是个标记，返回 None。
pub(crate) fn compaction_item_replay_text(item: &Value) -> Option<String> {
    let encrypted_content = compaction_encrypted_content(item)?;
    Some(match decode_compaction_summary(encrypted_content) {
        Some(summary) => format!("{SUMMARY_PREFIX}\n{summary}"),
        None => OPAQUE_COMPACTION_NOTE.to_string(),
    })
}

pub(crate) fn user_message_item(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{ "type": "input_text", "text": text }]
    })
}

/// 压缩请求里 `compaction_trigger` 所在位置换成的那条用户消息。
pub(crate) fn compaction_prompt_item() -> Value {
    user_message_item(COMPACT_PROMPT)
}

/// 压缩回合要交回 Codex 的唯一一个 compaction 条目。
pub(crate) fn compaction_output_item(summary: &str) -> Value {
    json!({
        "type": "compaction",
        "id": format!("cmp_{}", uuid::Uuid::new_v4().simple()),
        "encrypted_content": encode_compaction_summary(summary)
    })
}

/// 从本回合的输出条目里取摘要正文：所有 assistant message 的文字按顺序拼起来。
pub(crate) fn summary_from_output_items<'a>(items: impl IntoIterator<Item = &'a Value>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(content) = item.get("content").and_then(Value::as_array) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) != Some("output_text") {
                continue;
            }
            if let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                parts.push(text);
            }
        }
    }
    parts.join("\n\n")
}

/// 转换来的压缩回合（Chat 的 `finish_reason`、Anthropic 的 `stop_reason`）能不能交回
/// 压缩条目：返回 None 表示摘要写完了，否则是 `response.incomplete` 要带的原因。
///
/// 普通回合把内容过滤等非正常结束当作完成，至多少一段回答；压缩回合一旦算完成，Codex
/// 就拿这段摘要覆盖整段历史，所以只认正常停下的几种写法，其余一律报未完成（Codex 会重试）。
/// 没有结束原因时按完成处理，和普通回合一致（流提前断开由各自的流处理合成截断）。
pub(crate) fn compaction_incomplete_reason(finish_reason: Option<&str>) -> Option<String> {
    let reason = finish_reason.map(str::trim).filter(|r| !r.is_empty())?;
    match reason.to_ascii_lowercase().as_str() {
        "stop" | "end_turn" | "stop_sequence" => None,
        "length" | "max_tokens" | "model_context_window_exceeded" => {
            Some("max_output_tokens".to_string())
        }
        // OpenAI 的 content_filter、智谱的 sensitive、Anthropic 的 refusal。
        "content_filter" | "sensitive" | "refusal" => Some("content_filter".to_string()),
        _ => Some(reason.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_envelope_round_trips_and_rejects_foreign_blobs() {
        let encoded = encode_compaction_summary("进度：修好了压缩\nnext: test");
        assert!(encoded.starts_with(CCSWITCH_COMPACTION_PREFIX));
        assert_eq!(
            decode_compaction_summary(&encoded).as_deref(),
            Some("进度：修好了压缩\nnext: test")
        );
        assert_eq!(decode_compaction_summary("gAAAAABopenai-blob"), None);
    }

    #[test]
    fn replay_text_decodes_own_summary_and_notes_foreign_blob() {
        let own = json!({
            "type": "compaction",
            "encrypted_content": encode_compaction_summary("the summary")
        });
        assert_eq!(
            compaction_item_replay_text(&own).as_deref(),
            Some(format!("{SUMMARY_PREFIX}\nthe summary").as_str())
        );
        let foreign = json!({ "type": "compaction_summary", "encrypted_content": "gAAAA" });
        assert_eq!(
            compaction_item_replay_text(&foreign).as_deref(),
            Some(OPAQUE_COMPACTION_NOTE)
        );
        let marker = json!({ "type": "context_compaction" });
        assert_eq!(compaction_item_replay_text(&marker), None);
    }

    #[test]
    fn compaction_turn_only_completes_on_a_normal_stop() {
        for normal in [
            None,
            Some("stop"),
            Some("end_turn"),
            Some("stop_sequence"),
            Some("STOP"),
        ] {
            assert_eq!(compaction_incomplete_reason(normal), None, "{normal:?}");
        }
        for (reason, expected) in [
            ("length", "max_output_tokens"),
            ("max_tokens", "max_output_tokens"),
            ("content_filter", "content_filter"),
            ("sensitive", "content_filter"),
            ("refusal", "content_filter"),
            (
                "insufficient_system_resource",
                "insufficient_system_resource",
            ),
            ("tool_calls", "tool_calls"),
        ] {
            assert_eq!(
                compaction_incomplete_reason(Some(reason)).as_deref(),
                Some(expected),
                "{reason}"
            );
        }
    }
}
