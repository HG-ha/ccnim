use serde::{Deserialize, Serialize};

/// One of the upstream "shapes" the proxy knows how to talk to. Each variant
/// represents a *protocol family*, not a specific vendor — a single variant
/// can be hosted at many different base URLs, each potentially with their
/// own model catalog. The user picks the variant per-key when they paste
/// the secret in, and supplies the actual base URL alongside.
///
/// The wire format is `snake_case` so the JSON config and the TS frontend
/// share a single string vocabulary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// NVIDIA NIM (`https://integrate.api.nvidia.com/v1`). OpenAI-compatible
    /// chat completions endpoint, with NIM-specific `nvapi-…` keys.
    #[default]
    Nim,
    /// Generic OpenAI-compatible Chat Completions provider — DeepSeek's
    /// OpenAI shim, Moonshot, Groq, OpenRouter, an in-house vLLM, …
    /// Anything that exposes `POST {base}/chat/completions` with the
    /// usual OpenAI streaming shape.
    OpenaiCompat,
    /// Anthropic-native Messages API — official `api.anthropic.com`, or
    /// any third party speaking the same `/v1/messages` SSE protocol
    /// (DeepSeek's anthropic shim, Zhipu, …). The proxy passes those
    /// requests through with minimal rewriting.
    AnthropicCompat,
}

impl ProviderKind {
    /// Stable upstream URL the GUI prefills when the user adds a key with
    /// this provider type. Users can override the value, but for the
    /// canonical hosted offering we want zero configuration.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Nim => "https://integrate.api.nvidia.com/v1",
            // Empty string means "no canonical default exists" — the GUI
            // will require the user to enter a base URL before save.
            Self::OpenaiCompat => "",
            Self::AnthropicCompat => "https://api.anthropic.com",
        }
    }

    /// Short label shown in the dashboard / key cards.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Nim => "NIM",
            Self::OpenaiCompat => "OpenAI 兼容",
            Self::AnthropicCompat => "Anthropic 兼容",
        }
    }

    /// True when key values for this provider must start with `nvapi-`.
    /// We deliberately keep this strict only for NIM — third-party keys
    /// for OpenAI/Anthropic-compat providers come in many shapes
    /// (`sk-…`, `sk-ant-…`, raw bearer tokens, etc.) and validating
    /// them here would lock out legitimate users.
    pub fn requires_nvapi_prefix(self) -> bool {
        matches!(self, Self::Nim)
    }
}

/// Strip a trailing `/` so callers can safely concatenate a known suffix
/// like `/chat/completions` or `/v1/messages` without producing `//`.
/// Empty input is returned as-is (callers will reject empty base URLs
/// before reaching the network layer).
pub fn normalize_base_url(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&ProviderKind::Nim).unwrap(),
            "\"nim\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::OpenaiCompat).unwrap(),
            "\"openai_compat\""
        );
        assert_eq!(
            serde_json::to_string(&ProviderKind::AnthropicCompat).unwrap(),
            "\"anthropic_compat\""
        );
    }

    #[test]
    fn nim_has_canonical_default_url() {
        assert_eq!(
            ProviderKind::Nim.default_base_url(),
            "https://integrate.api.nvidia.com/v1"
        );
    }

    #[test]
    fn openai_compat_has_no_default_url_so_gui_forces_input() {
        assert!(ProviderKind::OpenaiCompat.default_base_url().is_empty());
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://example.com/v1/"),
            "https://example.com/v1"
        );
        assert_eq!(
            normalize_base_url("  https://example.com/v1//  "),
            "https://example.com/v1"
        );
        assert_eq!(normalize_base_url(""), "");
    }
}
