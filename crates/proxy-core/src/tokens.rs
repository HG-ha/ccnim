use crate::anthropic::{Content, ContentBlock, Message, System, Tool};

pub fn count_input_tokens(
    messages: &[Message],
    system: Option<&System>,
    tools: Option<&[Tool]>,
) -> usize {
    let mut text = String::new();
    if let Some(system) = system {
        match system {
            System::Text(value) => text.push_str(value),
            System::Blocks(blocks) => {
                for block in blocks {
                    text.push_str(&block.text);
                }
            }
        }
    }
    for message in messages {
        match &message.content {
            Content::Text(value) => text.push_str(value),
            Content::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text: value } => text.push_str(value),
                        ContentBlock::ToolUse { name, input, .. } => {
                            text.push_str(name);
                            text.push_str(&input.to_string());
                        }
                        ContentBlock::ToolResult { content, .. } => {
                            text.push_str(&content.to_string())
                        }
                        ContentBlock::Thinking { thinking, .. } => text.push_str(thinking),
                        ContentBlock::Image { .. } => text.push_str("[image]"),
                        ContentBlock::RedactedThinking { data } => text.push_str(data),
                    }
                }
            }
        }
    }
    if let Some(tools) = tools {
        for tool in tools {
            text.push_str(&tool.name);
            if let Some(description) = &tool.description {
                text.push_str(description);
            }
            if let Some(schema) = &tool.input_schema {
                text.push_str(&schema.to_string());
            }
        }
    }
    estimate_tokens(&text)
}

pub fn estimate_tokens(text: &str) -> usize {
    (text.chars().count() / 4).max(1)
}
