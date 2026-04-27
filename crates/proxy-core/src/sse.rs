use std::collections::HashMap;

use serde_json::{json, Value};
use uuid::Uuid;

use crate::openai::{ChatCompletionChunk, ToolCallDelta, Usage};

pub fn map_stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        Some("content_filter") => "end_turn",
        Some("stop") | None => "end_turn",
        Some(_) => "end_turn",
    }
}

/// Per-OpenAI-tool-call bookkeeping. The OpenAI streaming protocol splits a
/// single tool call across many chunks: the first chunk carries `id` and
/// `function.name`, subsequent chunks only carry `function.arguments`
/// fragments and rely on the shared `index` for grouping. Anthropic's
/// streaming protocol on the other hand wants exactly one
/// `content_block_start` (with the resolved `name`) followed by repeated
/// `input_json_delta` events for the args, then one `content_block_stop`.
///
/// Each slot tracks the bridge state until we have enough info (a non-empty
/// `name`) to emit `content_block_start`. Args that arrive before that point
/// are buffered and flushed in the same emit batch.
#[derive(Debug, Default)]
struct ToolCallSlot {
    content_index: usize,
    started: bool,
    pending_name: String,
    pending_id: Option<String>,
    pending_args: String,
}

#[derive(Debug)]
pub struct SseBuilder {
    message_id: String,
    model: String,
    input_tokens: usize,
    next_index: usize,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    text_started: bool,
    thinking_started: bool,
    accumulated_text: String,
    accumulated_thinking: String,
    /// Keyed by the OpenAI `tool_calls[i].index` so fragments collapse
    /// onto the same Anthropic `content_block`.
    tool_slots: HashMap<usize, ToolCallSlot>,
}

impl SseBuilder {
    pub fn new(model: impl Into<String>, input_tokens: usize) -> Self {
        Self {
            message_id: format!("msg_{}", Uuid::new_v4()),
            model: model.into(),
            input_tokens,
            next_index: 0,
            text_index: None,
            thinking_index: None,
            text_started: false,
            thinking_started: false,
            accumulated_text: String::new(),
            accumulated_thinking: String::new(),
            tool_slots: HashMap::new(),
        }
    }

    pub fn message_start(&self) -> String {
        self.event(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.input_tokens,
                        "output_tokens": 1
                    }
                }
            }),
        )
    }

    pub fn apply_chunk(&mut self, chunk: ChatCompletionChunk) -> Vec<String> {
        let mut out = Vec::new();
        let Some(choice) = chunk.choices.into_iter().next() else {
            return out;
        };

        if let Some(reasoning) = choice.delta.reasoning_content {
            out.extend(self.ensure_thinking());
            self.accumulated_thinking.push_str(&reasoning);
            out.push(self.content_delta(
                self.thinking_index.unwrap_or(0),
                "thinking_delta",
                json!({ "thinking": reasoning }),
            ));
        }

        if let Some(content) = choice.delta.content {
            let parts = split_thinking_tags(&content);
            for part in parts {
                match part {
                    TextPart::Text(text) if !text.is_empty() => {
                        out.extend(self.ensure_text());
                        self.accumulated_text.push_str(&text);
                        out.push(self.content_delta(
                            self.text_index.unwrap_or(0),
                            "text_delta",
                            json!({ "text": text }),
                        ));
                    }
                    TextPart::Thinking(text) if !text.is_empty() => {
                        out.extend(self.ensure_thinking());
                        self.accumulated_thinking.push_str(&text);
                        out.push(self.content_delta(
                            self.thinking_index.unwrap_or(0),
                            "thinking_delta",
                            json!({ "thinking": text }),
                        ));
                    }
                    _ => {}
                }
            }
        }

        if let Some(tool_calls) = choice.delta.tool_calls {
            for tool_call in tool_calls {
                out.extend(self.tool_call(tool_call));
            }
        }

        out
    }

    pub fn finish(&mut self, stop_reason: Option<&str>, usage: Option<Usage>) -> Vec<String> {
        let mut out = self.close_content_blocks();
        out.extend(self.close_tool_blocks());
        if self.accumulated_text.is_empty()
            && self.accumulated_thinking.is_empty()
            && self.tool_slots.is_empty()
        {
            out.extend(self.ensure_text());
            out.push(self.content_delta(
                self.text_index.unwrap_or(0),
                "text_delta",
                json!({ "text": " " }),
            ));
            out.extend(self.close_content_blocks());
        }
        let output_tokens = usage.and_then(|u| u.completion_tokens).unwrap_or_else(|| {
            estimate_text_tokens(&self.accumulated_text)
                + estimate_text_tokens(&self.accumulated_thinking)
        });
        out.push(self.event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": map_stop_reason(stop_reason),
                    "stop_sequence": null
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": output_tokens
                }
            }),
        ));
        out.push(self.event("message_stop", json!({ "type": "message_stop" })));
        out
    }

    pub fn error(&mut self, message: &str) -> Vec<String> {
        let mut out = self.close_content_blocks();
        out.extend(self.close_tool_blocks());
        out.extend(self.ensure_text());
        out.push(self.content_delta(
            self.text_index.unwrap_or(0),
            "text_delta",
            json!({ "text": message }),
        ));
        out.extend(self.close_content_blocks());
        out
    }

    fn ensure_text(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.thinking_started {
            out.push(self.content_stop(self.thinking_index.unwrap_or(0)));
            self.thinking_started = false;
        }
        if !self.text_started {
            let index = self.allocate_index();
            self.text_index = Some(index);
            self.text_started = true;
            out.push(self.content_start(index, json!({ "type": "text", "text": "" })));
        }
        out
    }

    fn ensure_thinking(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.text_started {
            out.push(self.content_stop(self.text_index.unwrap_or(0)));
            self.text_started = false;
        }
        if !self.thinking_started {
            let index = self.allocate_index();
            self.thinking_index = Some(index);
            self.thinking_started = true;
            out.push(self.content_start(index, json!({ "type": "thinking", "thinking": "" })));
        }
        out
    }

    fn close_content_blocks(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.thinking_started {
            out.push(self.content_stop(self.thinking_index.unwrap_or(0)));
            self.thinking_started = false;
        }
        if self.text_started {
            out.push(self.content_stop(self.text_index.unwrap_or(0)));
            self.text_started = false;
        }
        out
    }

    fn close_tool_blocks(&mut self) -> Vec<String> {
        let mut to_stop: Vec<usize> = self
            .tool_slots
            .values()
            .filter(|slot| slot.started)
            .map(|slot| slot.content_index)
            .collect();
        to_stop.sort_unstable();
        let mut out = Vec::with_capacity(to_stop.len());
        for idx in &to_stop {
            out.push(self.content_stop(*idx));
        }
        for slot in self.tool_slots.values_mut() {
            slot.started = false;
        }
        out
    }

    /// Apply one OpenAI tool-call delta. Multiple deltas with the same
    /// `tool_call.index` collapse into a single Anthropic `tool_use` block:
    /// `content_block_start` is emitted once (when we have a name), then
    /// each new `arguments` fragment becomes an `input_json_delta` carrying
    /// just that fragment as `partial_json`. Anthropic's client concatenates
    /// `partial_json` across deltas to reconstruct the full JSON args.
    fn tool_call(&mut self, tool_call: ToolCallDelta) -> Vec<String> {
        let openai_index = tool_call.index;
        let new_name = tool_call
            .function
            .as_ref()
            .and_then(|f| f.name.clone())
            .unwrap_or_default();
        let new_args = tool_call
            .function
            .as_ref()
            .and_then(|f| f.arguments.clone())
            .unwrap_or_default();
        let incoming_id = tool_call.id;

        // Pull the slot out so we can call `&mut self` helpers below.
        let mut slot = self.tool_slots.remove(&openai_index).unwrap_or_default();

        if !new_name.is_empty() {
            slot.pending_name.push_str(&new_name);
        }
        if slot.pending_id.is_none() && incoming_id.is_some() {
            slot.pending_id = incoming_id;
        }

        let mut out = Vec::new();

        if !slot.started {
            // Buffer args until we've seen a name and emitted start.
            slot.pending_args.push_str(&new_args);

            if !slot.pending_name.is_empty() {
                out.extend(self.close_content_blocks());

                slot.content_index = self.allocate_index();
                let id = slot
                    .pending_id
                    .clone()
                    .unwrap_or_else(|| format!("tool_{}", Uuid::new_v4()));
                out.push(self.content_start(
                    slot.content_index,
                    json!({
                        "type": "tool_use",
                        "id": id,
                        "name": slot.pending_name,
                        "input": {}
                    }),
                ));
                slot.started = true;

                if !slot.pending_args.is_empty() {
                    let buffered = std::mem::take(&mut slot.pending_args);
                    out.push(self.content_delta(
                        slot.content_index,
                        "input_json_delta",
                        json!({ "partial_json": buffered }),
                    ));
                }
            }
        } else if !new_args.is_empty() {
            out.push(self.content_delta(
                slot.content_index,
                "input_json_delta",
                json!({ "partial_json": new_args }),
            ));
        }

        self.tool_slots.insert(openai_index, slot);
        out
    }

    fn allocate_index(&mut self) -> usize {
        let index = self.next_index;
        self.next_index += 1;
        index
    }

    fn content_start(&self, index: usize, content_block: Value) -> String {
        self.event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block
            }),
        )
    }

    fn content_delta(&self, index: usize, delta_type: &str, mut delta: Value) -> String {
        if let Value::Object(map) = &mut delta {
            map.insert("type".to_string(), Value::String(delta_type.to_string()));
        }
        self.event(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": delta
            }),
        )
    }

    fn content_stop(&self, index: usize) -> String {
        self.event(
            "content_block_stop",
            json!({
                "type": "content_block_stop",
                "index": index
            }),
        )
    }

    fn event(&self, name: &str, data: Value) -> String {
        format!("event: {name}\ndata: {data}\n\n")
    }
}

enum TextPart {
    Text(String),
    Thinking(String),
}

fn split_thinking_tags(input: &str) -> Vec<TextPart> {
    let Some(start) = input.find("<think>") else {
        return vec![TextPart::Text(input.to_string())];
    };
    let Some(end) = input.find("</think>") else {
        return vec![TextPart::Text(input.to_string())];
    };
    let mut parts = Vec::new();
    if start > 0 {
        parts.push(TextPart::Text(input[..start].to_string()));
    }
    parts.push(TextPart::Thinking(input[start + 7..end].trim().to_string()));
    if end + 8 < input.len() {
        parts.push(TextPart::Text(input[end + 8..].to_string()));
    }
    parts
}

pub fn estimate_text_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::{Choice, Delta, FunctionDelta};

    fn chunk_with_tool(deltas: Vec<ToolCallDelta>) -> ChatCompletionChunk {
        ChatCompletionChunk {
            choices: vec![Choice {
                index: 0,
                delta: Delta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(deltas),
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    /// Regression: NIM streams tool calls in fragments, where chunks 2+
    /// carry only `function.arguments` and no `name` / `id`. The pre-fix
    /// builder emitted a *new* `tool_use` block for every fragment, with a
    /// fallback name of `"tool_call"`. Claude Code then rejected those
    /// fragments with `No such tool available: tool_call`. The fixed
    /// builder must emit exactly one `content_block_start` (with the real
    /// name) and stream args as `input_json_delta` fragments.
    #[test]
    fn streaming_tool_call_fragments_collapse_to_one_block() {
        let mut sse = SseBuilder::new("model", 0);

        // Chunk 1: name + id, args still empty.
        let frames = sse.apply_chunk(chunk_with_tool(vec![ToolCallDelta {
            index: 0,
            id: Some("call_abc".into()),
            r#type: Some("function".into()),
            function: Some(FunctionDelta {
                name: Some("Glob".into()),
                arguments: Some(String::new()),
            }),
        }]));
        let blob = frames.join("");
        assert!(
            blob.contains("\"name\":\"Glob\""),
            "first chunk must emit content_block_start with the real tool name; got: {blob}"
        );
        assert!(
            !blob.contains("\"name\":\"tool_call\""),
            "the placeholder name must never leak to the client; got: {blob}"
        );

        // Chunks 2/3: streaming args fragments only.
        let frames = sse.apply_chunk(chunk_with_tool(vec![ToolCallDelta {
            index: 0,
            id: None,
            r#type: None,
            function: Some(FunctionDelta {
                name: None,
                arguments: Some("{\"pattern\":".into()),
            }),
        }]));
        let blob = frames.join("");
        assert!(
            blob.contains("input_json_delta") && blob.contains("{\\\"pattern\\\":"),
            "args fragment should stream as input_json_delta; got: {blob}"
        );
        assert!(
            !blob.contains("content_block_start"),
            "subsequent fragments must not open new blocks; got: {blob}"
        );

        let frames = sse.apply_chunk(chunk_with_tool(vec![ToolCallDelta {
            index: 0,
            id: None,
            r#type: None,
            function: Some(FunctionDelta {
                name: None,
                arguments: Some("\"*.rs\"}".into()),
            }),
        }]));
        let blob = frames.join("");
        assert!(blob.contains("input_json_delta"));
        assert!(!blob.contains("content_block_start"));

        // finish() must close the tool block exactly once. Count the
        // `event:` header rather than raw substring matches because each
        // SSE event repeats its name in the `data:` JSON `type` field.
        let frames = sse.finish(Some("tool_calls"), None);
        let blob = frames.join("");
        let stop_count = blob.matches("event: content_block_stop").count();
        assert_eq!(
            stop_count, 1,
            "finish() must emit exactly one content_block_stop for the tool block; got {stop_count} in {blob}"
        );
        assert!(blob.contains("\"stop_reason\":\"tool_use\""));
    }

    /// Two tool calls multiplexed by index across the same stream should
    /// produce two distinct `content_block_start` events with the right
    /// names, and two stops on finish.
    #[test]
    fn multiple_tool_calls_keep_separate_indices() {
        let mut sse = SseBuilder::new("model", 0);

        sse.apply_chunk(chunk_with_tool(vec![
            ToolCallDelta {
                index: 0,
                id: Some("call_a".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("Read".into()),
                    arguments: Some(String::new()),
                }),
            },
            ToolCallDelta {
                index: 1,
                id: Some("call_b".into()),
                r#type: Some("function".into()),
                function: Some(FunctionDelta {
                    name: Some("Glob".into()),
                    arguments: Some(String::new()),
                }),
            },
        ]));

        sse.apply_chunk(chunk_with_tool(vec![ToolCallDelta {
            index: 1,
            id: None,
            r#type: None,
            function: Some(FunctionDelta {
                name: None,
                arguments: Some("{\"pattern\":\"*\"}".into()),
            }),
        }]));

        let frames = sse.finish(Some("tool_calls"), None);
        let blob = frames.join("");
        assert_eq!(blob.matches("event: content_block_stop").count(), 2);
        assert!(!blob.contains("\"name\":\"tool_call\""));
    }
}
