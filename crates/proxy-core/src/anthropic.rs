use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: Value },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult { tool_use_id: String, content: Value },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SystemContent {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ThinkingConfig {
    /// Legacy / non-spec field that some experimental clients emit
    /// (`{"thinking":{"enabled":true}}`). The official Anthropic
    /// Messages API uses `type: "enabled" | "disabled"` as a
    /// discriminator and rejects an `enabled` key on the
    /// `ThinkingEnabled` subtype with the wonderfully cryptic
    /// "thinking.enabled.enabled: Extra inputs are not permitted".
    /// Accept it for compatibility with whatever sent it, but never
    /// emit it on the wire (`skip_serializing_if`) so passthrough
    /// requests stay valid against real Anthropic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<System>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum System {
    Text(String),
    Blocks(Vec<SystemContent>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenCountRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<System>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenCountResponse {
    pub input_tokens: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelResponse {
    pub id: String,
    pub display_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelsListResponse {
    pub object: String,
    pub data: Vec<ModelResponse>,
    pub first_id: Option<String>,
    pub has_more: bool,
    pub last_id: Option<String>,
}

/// Strip the `thinking` field from an outgoing Messages body. Used on
/// the Anthropic-passthrough path when the user has explicitly turned
/// extended thinking off in CCNim's config but the client (e.g.
/// Claude Code, Cursor) keeps emitting `"thinking": {...}`. Some third-
/// party Anthropic-compatible gateways reject `thinking` outright, so
/// honouring the toggle here is the only escape hatch the user has.
pub fn strip_thinking(body: &mut Value) {
    if let Value::Object(map) = body {
        map.remove("thinking");
    }
}

/// Mutates an outgoing `/v1/messages` JSON body so it satisfies Anthropic's
/// extended-thinking rule: `max_tokens` must be **strictly greater** than
/// `thinking.budget_tokens`.
///
/// Hand-written tests (and occasionally mis-synced clients) set both to the
/// same integer; Anthropic rejects those with HTTP 400. We bump `max_tokens`
/// by the smallest amount that satisfies the constraint.
pub fn normalize_max_tokens_for_extended_thinking(body: &mut Value) {
    let Value::Object(map) = body else {
        return;
    };
    let budget = match map.get("thinking") {
        Some(Value::Object(th)) => th.get("budget_tokens").and_then(|v| v.as_u64()),
        _ => None,
    };
    let Some(budget) = budget else {
        return;
    };

    let Some(Value::Number(n)) = map.get("max_tokens") else {
        return;
    };
    let Some(max) = n.as_u64() else {
        return;
    };

    if max <= budget {
        map.insert(
            "max_tokens".into(),
            Value::Number((budget.saturating_add(1)).into()),
        );
    }
}

impl ModelsListResponse {
    pub fn claude_compatible() -> Self {
        let ids = [
            ("claude-opus-4-20250514", "Claude Opus 4"),
            ("claude-sonnet-4-20250514", "Claude Sonnet 4"),
            ("claude-haiku-4-20250514", "Claude Haiku 4"),
            ("claude-3-opus-20240229", "Claude 3 Opus"),
            ("claude-3-5-sonnet-20241022", "Claude 3.5 Sonnet"),
            ("claude-3-haiku-20240307", "Claude 3 Haiku"),
            ("claude-3-5-haiku-20241022", "Claude 3.5 Haiku"),
        ];
        let data = ids
            .into_iter()
            .map(|(id, display_name)| ModelResponse {
                id: id.to_string(),
                display_name: display_name.to_string(),
                created_at: "2025-05-14T00:00:00Z".to_string(),
            })
            .collect::<Vec<_>>();
        Self {
            object: "list".to_string(),
            first_id: data.first().map(|m| m.id.clone()),
            last_id: data.last().map(|m| m.id.clone()),
            has_more: false,
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a request body that arrives with the official
    /// `{"thinking":{"type":"enabled","budget_tokens":N}}` shape must
    /// round-trip through deserialize → serialize *without* gaining a
    /// spurious `"enabled":null` field, otherwise real Anthropic
    /// rejects the passthrough with
    /// `thinking.enabled.enabled: Extra inputs are not permitted`.
    #[test]
    fn thinking_config_does_not_leak_enabled_null_on_passthrough() {
        let raw = r#"{"thinking":{"type":"enabled","budget_tokens":1024}}"#;
        #[derive(Deserialize, Serialize)]
        struct Wrap {
            thinking: ThinkingConfig,
        }
        let parsed: Wrap = serde_json::from_str(raw).expect("valid input");
        let reserialized = serde_json::to_string(&parsed).unwrap();
        // We check for the field-key form `"enabled":` rather than the
        // substring "enabled", because the legitimate value `"type":"enabled"`
        // would otherwise produce a false positive.
        assert!(
            !reserialized.contains(r#""enabled":"#),
            "ThinkingConfig must not emit an `enabled` field when not present \
             in input; got: {reserialized}"
        );
        assert!(reserialized.contains(r#""type":"enabled""#));
        assert!(reserialized.contains(r#""budget_tokens":1024"#));
    }

    /// If a client *did* explicitly send `enabled`, we still preserve
    /// it on the way out (some experimental upstreams accept it).
    #[test]
    fn thinking_config_preserves_explicit_enabled_field() {
        let raw = r#"{"enabled":true,"budget_tokens":256}"#;
        let parsed: ThinkingConfig = serde_json::from_str(raw).unwrap();
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert!(reserialized.contains(r#""enabled":true"#));
    }

    #[test]
    fn normalize_max_tokens_bumps_when_equal_to_budget() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "thinking": { "type": "enabled", "budget_tokens": 1024 },
            "messages": []
        });
        normalize_max_tokens_for_extended_thinking(&mut body);
        assert_eq!(body["max_tokens"], 1025);
    }

    #[test]
    fn strip_thinking_removes_field_when_present() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "thinking": { "type": "enabled", "budget_tokens": 512 },
            "messages": []
        });
        strip_thinking(&mut body);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn strip_thinking_is_noop_when_field_absent() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": []
        });
        strip_thinking(&mut body);
        assert_eq!(body["max_tokens"], 1024);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn normalize_max_tokens_noop_when_already_above_budget() {
        let mut body = serde_json::json!({
            "max_tokens": 2048,
            "thinking": { "type": "enabled", "budget_tokens": 1024 },
        });
        normalize_max_tokens_for_extended_thinking(&mut body);
        assert_eq!(body["max_tokens"], 2048);
    }
}
