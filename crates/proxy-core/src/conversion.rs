use serde_json::{json, Map, Value};

use crate::anthropic::{Content, ContentBlock, MessagesRequest, Role, System, Tool};
use crate::openai::{ChatCompletionRequest, ChatMessage};

#[derive(Debug, Clone)]
pub struct NimRequestOptions {
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub enable_thinking: bool,
}

impl Default for NimRequestOptions {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            enable_thinking: true,
        }
    }
}

pub fn anthropic_to_nim(
    request: &MessagesRequest,
    options: &NimRequestOptions,
) -> ChatCompletionRequest {
    let mut messages = Vec::new();
    if let Some(system) = &request.system {
        if let Some(text) = system_to_text(system) {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(text),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
            });
        }
    }

    for message in &request.messages {
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        match &message.content {
            Content::Text(text) => messages.push(ChatMessage {
                role: role.to_string(),
                content: Some(text.clone()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: message.reasoning_content.clone(),
            }),
            Content::Blocks(blocks) if matches!(message.role, Role::User) => {
                append_user_blocks(&mut messages, blocks);
            }
            Content::Blocks(blocks) => messages.push(assistant_blocks_to_message(blocks)),
        }
    }

    let mut extra = Map::new();
    if let Some(Value::Object(map)) = &request.extra_body {
        extra.extend(map.clone());
    }
    if options.enable_thinking {
        extra
            .entry("chat_template_kwargs".to_string())
            .or_insert_with(|| {
                json!({
                    "thinking": true,
                    "enable_thinking": true,
                    "reasoning_budget": request.max_tokens.unwrap_or(options.max_tokens)
                })
            });
    }

    ChatCompletionRequest {
        model: request.model.clone(),
        messages,
        max_tokens: Some(request.max_tokens.unwrap_or(options.max_tokens)),
        temperature: request.temperature.or(options.temperature),
        top_p: request.top_p.or(options.top_p),
        stop: request.stop_sequences.clone(),
        tools: request.tools.as_ref().map(|tools| convert_tools(tools)),
        tool_choice: request.tool_choice.clone(),
        stream: true,
        stream_options: Some(json!({ "include_usage": true })),
        extra,
    }
}

fn system_to_text(system: &System) -> Option<String> {
    match system {
        System::Text(text) => Some(text.clone()),
        System::Blocks(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.kind == "text")
                .map(|block| block.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            (!text.is_empty()).then_some(text)
        }
    }
}

fn append_user_blocks(messages: &mut Vec<ChatMessage>, blocks: &[ContentBlock]) {
    let mut text_parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::ToolResult {
                tool_use_id,
                content,
            } => {
                if !text_parts.is_empty() {
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: Some(text_parts.join("\n")),
                        tool_call_id: None,
                        tool_calls: None,
                        reasoning_content: None,
                    });
                    text_parts.clear();
                }
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: Some(tool_result_to_text(content)),
                    tool_call_id: Some(tool_use_id.clone()),
                    tool_calls: None,
                    reasoning_content: None,
                });
            }
            _ => {}
        }
    }
    if !text_parts.is_empty() {
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(text_parts.join("\n")),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        });
    }
}

fn assistant_blocks_to_message(blocks: &[ContentBlock]) -> ChatMessage {
    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => content_parts.push(text.clone()),
            ContentBlock::Thinking { thinking, .. } => {
                content_parts.push(format!("<think>\n{thinking}\n</think>"));
            }
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": input.to_string()
                }
            })),
            _ => {}
        }
    }
    ChatMessage {
        role: "assistant".to_string(),
        content: Some(if content_parts.is_empty() && tool_calls.is_empty() {
            " ".to_string()
        } else {
            content_parts.join("\n\n")
        }),
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        reasoning_content: None,
    }
}

fn tool_result_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => value.to_string(),
    }
}

fn convert_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description.clone().unwrap_or_default(),
                    "parameters": tool.input_schema.clone().unwrap_or_else(|| json!({
                        "type": "object",
                        "properties": {}
                    }))
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::anthropic::{Content, Message, Role};

    use super::*;

    #[test]
    fn converts_basic_message_to_nim_chat_request() {
        let request = MessagesRequest {
            model: "deepseek-ai/deepseek-v4-flash".to_string(),
            max_tokens: Some(100),
            messages: vec![Message {
                role: Role::User,
                content: Content::Text("hello".to_string()),
                reasoning_content: None,
            }],
            system: Some(System::Text("be useful".to_string())),
            stop_sequences: None,
            stream: Some(true),
            temperature: Some(0.2),
            top_p: None,
            top_k: None,
            metadata: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            extra_body: None,
        };

        let body = anthropic_to_nim(&request, &NimRequestOptions::default());
        assert_eq!(body.model, "deepseek-ai/deepseek-v4-flash");
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[1].content.as_deref(), Some("hello"));
        assert!(body.stream);
    }
}
