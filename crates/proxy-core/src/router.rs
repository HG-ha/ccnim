#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMapping {
    pub default_model: String,
    pub opus_model: Option<String>,
    pub sonnet_model: Option<String>,
    pub haiku_model: Option<String>,
}

impl ModelMapping {
    pub fn resolve(&self, claude_model_name: &str) -> String {
        let lower = claude_model_name.to_lowercase();
        if lower.contains("opus") {
            if let Some(model) = &self.opus_model {
                return model.clone();
            }
        }
        if lower.contains("haiku") {
            if let Some(model) = &self.haiku_model {
                return model.clone();
            }
        }
        if lower.contains("sonnet") {
            if let Some(model) = &self.sonnet_model {
                return model.clone();
            }
        }
        self.default_model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_specific_claude_model_families() {
        let mapping = ModelMapping {
            default_model: "deepseek-ai/deepseek-v4-flash".to_string(),
            opus_model: Some("moonshotai/kimi-k2.5".to_string()),
            sonnet_model: Some("z-ai/glm4.7".to_string()),
            haiku_model: None,
        };

        assert_eq!(
            mapping.resolve("claude-opus-4-20250514"),
            "moonshotai/kimi-k2.5"
        );
        assert_eq!(mapping.resolve("claude-sonnet-4-20250514"), "z-ai/glm4.7");
        assert_eq!(
            mapping.resolve("claude-3-haiku-20240307"),
            "deepseek-ai/deepseek-v4-flash"
        );
    }
}
