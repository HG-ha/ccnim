use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use proxy_core::{normalize_base_url, ModelMapping, ProviderKind};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default auth token used when none is configured. Not a real secret; it just
/// lets the local proxy reject completely unauthenticated callers.
const DEFAULT_AUTH_TOKEN: &str = "freecc";

/// One configured upstream credential — a key/secret plus the upstream
/// base URL it talks to and the protocol family ("provider") it belongs
/// to. The GUI calls this an "API Key" for short.
///
/// `id` is a stable per-key identifier (UUID v4 string) that the GUI uses for
/// edit/delete operations; the secret material itself stays in `value`.
/// `label` is a free-form note (e.g. "personal account / dev"). `expires_at`
/// is unix-epoch seconds, `None` means "never expires".
///
/// `provider` selects the wire protocol. `base_url` overrides the canonical
/// upstream URL for that provider (useful for OpenAI-compatible third-party
/// hosts and self-hosted deployments). When `base_url` is empty, the proxy
/// falls back to [`ProviderKind::default_base_url`].
///
/// `enabled` controls whether this key is actively used for routing.
/// Disabled keys are skipped during key selection but kept in the config.
///
/// Note: the type is named `NimApiKey` for source-level backwards
/// compatibility with the original NIM-only build. New callers are
/// encouraged to use the alias [`UpstreamKey`] instead.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct NimApiKey {
    pub id: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// Protocol family. Defaults to NIM for backwards compatibility with
    /// pre-multi-provider configs.
    #[serde(default)]
    pub provider: ProviderKind,
    /// Upstream base URL. Empty means "use the default for this provider".
    /// Stored verbatim (modulo trailing-slash normalization on save) so we
    /// don't surprise users by mutating their input.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    /// Whether this key is enabled. Defaults to true for backwards compatibility.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-key override for the global `AppConfig.model_mapping`. Each
    /// field falls back to the global mapping independently when left
    /// `None` / empty, so a user can override only `sonnet_model` for a
    /// given upstream without having to repeat the rest of the mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_mapping: Option<PerKeyModelMappingConfig>,
    /// Per-key request rate limit (requests per `rate_window_secs`).
    /// When `None`, the runtime falls back to a provider-specific
    /// default — NIM uses the global `AppConfig.rate_limit_per_key`
    /// (matching the upstream's published 40 RPM cap), while
    /// OpenAI- / Anthropic-compatible upstreams default to *no*
    /// per-key limit because their published quotas vary widely
    /// between hosts and most users would rather pin them themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u32>,
}

fn default_true() -> bool {
    true
}

/// Modern alias for the structured upstream key. New code should prefer
/// this name; the [`NimApiKey`] alias is kept to avoid churning every
/// import site at once.
pub type UpstreamKey = NimApiKey;

impl NimApiKey {
    pub fn from_value(value: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            value: value.into(),
            label: None,
            expires_at: None,
            provider: ProviderKind::default(),
            base_url: String::new(),
            enabled: true,
            model_mapping: None,
            rate_limit: None,
        }
    }

    /// Resolve the effective base URL for this key — user-supplied value
    /// if non-empty, otherwise the canonical default for the configured
    /// provider. The returned string never has a trailing `/`.
    pub fn effective_base_url(&self) -> String {
        if self.base_url.trim().is_empty() {
            normalize_base_url(self.provider.default_base_url())
        } else {
            normalize_base_url(&self.base_url)
        }
    }

    /// Resolve the per-key rate limit (requests per the configured
    /// window). Returns `None` when the key should be treated as
    /// unlimited.
    ///
    /// Resolution order:
    ///   1. User-supplied [`Self::rate_limit`] takes precedence.
    ///      A zero is treated as "unlimited" rather than "block all";
    ///      "block all" isn't a useful UX and a 0 in a number input
    ///      almost always means the user wants the cap removed.
    ///   2. Otherwise the provider default applies — NIM keeps the
    ///      historic global cap (`default_nim_limit`), and OpenAI- /
    ///      Anthropic-compatible upstreams are unlimited so quota
    ///      handling lives entirely on the upstream's side until the
    ///      user opts in.
    pub fn effective_rate_limit(&self, default_nim_limit: usize) -> Option<usize> {
        if let Some(n) = self.rate_limit {
            if n == 0 {
                None
            } else {
                Some(n as usize)
            }
        } else {
            match self.provider {
                ProviderKind::Nim => Some(default_nim_limit),
                ProviderKind::OpenaiCompat | ProviderKind::AnthropicCompat => None,
            }
        }
    }
}

/// Public, non-secret configuration that is safe to keep as plaintext JSON.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    /// Stored separately on disk (see [`secrets_path`]), but exposed here so
    /// the GUI can edit it like any other field. Round-trips through Tauri IPC.
    #[serde(default)]
    pub auth_token: String,
    /// Stored separately on disk (see [`secrets_path`]).
    ///
    /// The custom deserializer accepts either the legacy `["nvapi-..."]` shape
    /// or the new structured `[{ id, value, label?, expires_at? }]` shape, so
    /// upgrading from earlier builds is silent.
    #[serde(default, deserialize_with = "deserialize_nim_keys")]
    pub nim_api_keys: Vec<NimApiKey>,
    pub model_mapping: ModelMappingConfig,
    pub rate_limit_per_key: usize,
    pub rate_window_secs: u64,
    pub enable_thinking: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct ModelMappingConfig {
    pub default_model: String,
    pub opus_model: Option<String>,
    pub sonnet_model: Option<String>,
    pub haiku_model: Option<String>,
}

/// Per-key model mapping override. All fields are optional because each
/// one falls back to the corresponding global field when left empty —
/// so the user can configure just the overrides that actually differ
/// for this upstream and inherit the rest. Empty strings are treated
/// as "not set" by the runtime, so the GUI can persist `""` without
/// regressing into a hard override.
///
/// Each slot also carries an optional `*_extra_body` JSON object —
/// arbitrary fields the user wants deep-merged into the outgoing
/// request body whenever this slot is the one that wins resolution.
/// Designed as an escape hatch: any upstream-specific knob without
/// dedicated GUI fields (`temperature`, `top_p`, `max_tokens`,
/// `top_k`, `thinking`, `metadata`, …) can be set by typing
/// free-form JSON into the GUI's "高级 / extra_body" textarea.
/// Config values *win* over anything the client (Claude Code) sent
/// for the same keys, so configuring this here makes the override
/// reliably take effect rather than being optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct PerKeyModelMappingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// `extra_body` deep-merged into requests routed through the
    /// `default_model` slot. Must be a JSON object (anything else is
    /// silently ignored at runtime to avoid sending malformed bodies
    /// upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_extra_body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus_extra_body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet_extra_body: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku_extra_body: Option<serde_json::Value>,
}

impl PerKeyModelMappingConfig {
    /// `true` when every field is unset / blank — i.e. there is no
    /// effective override at all, and the runtime can skip the merge
    /// entirely. Lets callers normalise `Some(empty)` into `None` on
    /// save so we don't litter `secrets.json` with no-op objects.
    pub fn is_empty(&self) -> bool {
        let blank_str = |s: &Option<String>| s.as_deref().map(str::trim).unwrap_or("").is_empty();
        let blank_json = |v: &Option<serde_json::Value>| match v {
            None => true,
            // An empty JSON object means "no overrides" — treat it as
            // unset so an explicit `{}` round-tripped from the GUI
            // doesn't pin a no-op object on disk forever.
            Some(serde_json::Value::Object(map)) => map.is_empty(),
            // Anything else (string, number, array, …) is technically
            // valid JSON but useless as an `extra_body` — surface it
            // as "set" so a hand-edited config.json doesn't get
            // silently swallowed by the GUI's normalisation.
            Some(_) => false,
        };
        blank_str(&self.default_model)
            && blank_str(&self.opus_model)
            && blank_str(&self.sonnet_model)
            && blank_str(&self.haiku_model)
            && blank_json(&self.default_extra_body)
            && blank_json(&self.opus_extra_body)
            && blank_json(&self.sonnet_extra_body)
            && blank_json(&self.haiku_extra_body)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8082,
            auth_token: DEFAULT_AUTH_TOKEN.to_string(),
            nim_api_keys: Vec::new(),
            model_mapping: ModelMappingConfig {
                default_model: "deepseek-ai/deepseek-v4-flash".to_string(),
                opus_model: None,
                sonnet_model: None,
                haiku_model: None,
            },
            rate_limit_per_key: 40,
            rate_window_secs: 60,
            enable_thinking: true,
        }
    }
}

impl From<ModelMappingConfig> for ModelMapping {
    fn from(value: ModelMappingConfig) -> Self {
        // The global mapping has no `extra_body` slots — those live
        // only on `PerKeyModelMappingConfig` for now. The runtime
        // `ModelMapping` carries the slot-specific extras as
        // `Option`s, which default to `None` here.
        Self {
            default_model: value.default_model,
            opus_model: value.opus_model,
            sonnet_model: value.sonnet_model,
            haiku_model: value.haiku_model,
            ..Default::default()
        }
    }
}

/// On-disk representation of secrets, kept in a separate file so it can be
/// permission-restricted and (eventually) replaced with an OS-keyed encrypted
/// blob without churning the public config schema.
///
/// Supports the legacy `Vec<String>` shape on read for forward-compatibility
/// with users upgrading from an earlier build where keys had no metadata.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct SecretsFile {
    #[serde(default)]
    auth_token: String,
    #[serde(default, deserialize_with = "deserialize_nim_keys")]
    nim_api_keys: Vec<NimApiKey>,
}

impl AppConfig {
    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.json"))
    }

    /// Path to the per-user secrets file (auth token + NIM keys). Lives in the
    /// same OS-defined config dir as `config.json`.
    pub fn secrets_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("secrets.json"))
    }

    /// Path to a small append-only diagnostic log, used to debug config IO
    /// when the GUI build hides stdout. Best-effort; failures are ignored.
    pub fn diagnostic_log_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("diagnostic.log"))
    }

    fn config_dir() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "ccnim", "CCNim")
            .context("could not resolve app config directory")?;
        Ok(dirs.config_dir().to_path_buf())
    }

    /// Sanity-check invariants every persisted config must satisfy.
    /// Centralised here so [`Self::save`] and any future external
    /// validation entry point share one source of truth, and so the
    /// error messages stay user-facing (Chinese, with a hint about
    /// which page to fix).
    ///
    /// Currently this only enforces that the global `default_model`
    /// is set, because that's the only field whose emptiness
    /// silently breaks request handling at runtime. Other invariants
    /// (port range, host shape, …) are already enforced by the GUI's
    /// own input types.
    pub fn validate_for_save(&self) -> Result<()> {
        if self.model_mapping.default_model.trim().is_empty() {
            anyhow::bail!("默认模型不能为空 — 请在「模型映射」页填写一个默认模型 ID 后再保存");
        }
        Ok(())
    }

    /// Load the persistent config and resolve secrets.
    ///
    /// Resolution order for `auth_token` and `nim_api_keys`:
    /// 1. `secrets.json` next to `config.json` (source of truth in steady state).
    /// 2. Plaintext fields embedded in `config.json` (legacy / migration).
    /// 3. `Default::default()` for the auth token; empty list for keys.
    ///
    /// When step 2 finds plaintext values, they are migrated to `secrets.json`
    /// immediately and the on-disk JSON is rewritten without them.
    pub fn load_or_default() -> Result<Self> {
        let config_path = Self::config_path()?;
        let secrets_path = Self::secrets_path()?;

        let (mut cfg, json_had_secrets) = if config_path.exists() {
            let contents = fs::read_to_string(&config_path)
                .with_context(|| format!("failed reading {}", config_path.display()))?;
            let raw: Self = serde_json::from_str(&contents)
                .with_context(|| format!("failed parsing {}", config_path.display()))?;
            let had = !raw.auth_token.is_empty() || !raw.nim_api_keys.is_empty();
            (raw, had)
        } else {
            (Self::default(), false)
        };

        let secrets_result = load_secrets_file(&secrets_path);
        diag(&format!(
            "load: config_exists={} legacy_secrets_in_config={} secrets_file={}",
            config_path.exists(),
            json_had_secrets,
            describe_secrets_result(&secrets_result),
        ));

        if let Ok(Some(secrets)) = &secrets_result {
            if !secrets.auth_token.is_empty() {
                cfg.auth_token = secrets.auth_token.clone();
            }
            if !secrets.nim_api_keys.is_empty() {
                cfg.nim_api_keys = secrets.nim_api_keys.clone();
            }
        }

        if cfg.auth_token.is_empty() {
            cfg.auth_token = DEFAULT_AUTH_TOKEN.to_string();
        }

        diag(&format!(
            "load: resolved auth_token_len={} nim_api_keys_count={}",
            cfg.auth_token.len(),
            cfg.nim_api_keys.len(),
        ));

        // Migration: if config.json carries plaintext secrets (legacy build), or
        // if the secrets file is missing/corrupt while we now hold non-default
        // secrets in-memory (because we just defaulted), persist a clean state.
        if json_had_secrets || secrets_result.is_err() {
            cfg.save()?;
        }

        Ok(cfg)
    }

    /// Persist the config: secrets go to `secrets.json` (with restrictive
    /// permissions on Unix), everything else goes to `config.json`. The JSON
    /// config file never contains the auth token or NIM keys.
    pub fn save(&self) -> Result<()> {
        // Validate up front so a malformed config can't slip through to
        // disk and silently break requests later. The default model is
        // the runtime's last-resort fallback when no per-key override
        // matches the incoming Claude model — saving an empty string
        // here would forward `""` upstream and reliably trip a 400 the
        // user has to puzzle out from upstream logs. Surface the error
        // *here* with a message that points at the right page.
        self.validate_for_save()?;

        let config_path = Self::config_path()?;
        let secrets_path = Self::secrets_path()?;

        diag(&format!(
            "save: auth_token_len={} nim_api_keys_count={}",
            self.auth_token.len(),
            self.nim_api_keys.len(),
        ));

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed creating {}", parent.display()))?;
        }

        let secrets = SecretsFile {
            auth_token: self.auth_token.clone(),
            nim_api_keys: self.nim_api_keys.clone(),
        };
        save_secrets_file(&secrets_path, &secrets)
            .inspect_err(|err| diag(&format!("save: secrets file write failed: {err:#}")))?;

        // Sanity-check the round-trip so the GUI surfaces a clear error if the
        // file system silently rejected the write (read-only volume, AV
        // quarantine, redirected folder, etc.) instead of failing silently.
        let verify = load_secrets_file(&secrets_path)
            .with_context(|| format!("failed verifying {}", secrets_path.display()))?
            .unwrap_or_default();
        if verify.auth_token != self.auth_token || verify.nim_api_keys != self.nim_api_keys {
            diag(&format!(
                "save: verify mismatch verify_token_len={} verify_keys_count={}",
                verify.auth_token.len(),
                verify.nim_api_keys.len(),
            ));
            anyhow::bail!(
                "secrets.json 写入后立即读回的值不一致，磁盘可能被只读挂载或被杀软拦截。\
                 请在 config_dir 下查看 diagnostic.log 获取详情。"
            );
        }

        let mut value =
            serde_json::to_value(self).context("failed encoding AppConfig as JSON value")?;
        if let Some(obj) = value.as_object_mut() {
            obj.remove("auth_token");
            obj.remove("nim_api_keys");
        }
        fs::write(&config_path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed writing {}", config_path.display()))?;

        diag("save: ok");
        Ok(())
    }

    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn load_secrets_file(path: &Path) -> Result<Option<SecretsFile>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents =
        fs::read_to_string(path).with_context(|| format!("failed reading {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(Some(SecretsFile::default()));
    }
    let secrets: SecretsFile = serde_json::from_str(&contents)
        .with_context(|| format!("failed parsing {}", path.display()))?;
    Ok(Some(secrets))
}

fn save_secrets_file(path: &Path, secrets: &SecretsFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(secrets).context("failed encoding secrets as JSON")?;
    fs::write(path, json).with_context(|| format!("failed writing {}", path.display()))?;
    apply_secrets_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn apply_secrets_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(path)
        .with_context(|| format!("failed reading metadata of {}", path.display()))?
        .permissions();
    perm.set_mode(0o600);
    fs::set_permissions(path, perm)
        .with_context(|| format!("failed setting mode 0600 on {}", path.display()))
}

#[cfg(not(unix))]
fn apply_secrets_permissions(_path: &Path) -> Result<()> {
    // Windows: %APPDATA% is per-user by default (NTFS ACL inheritance from the
    // user's profile). No additional ACL hardening needed for the intended
    // threat model. macOS path goes through the cfg(unix) branch above.
    Ok(())
}

fn describe_secrets_result(result: &Result<Option<SecretsFile>>) -> String {
    match result {
        Ok(Some(secrets)) => format!(
            "present(token_len={}, keys={})",
            secrets.auth_token.len(),
            secrets.nim_api_keys.len()
        ),
        Ok(None) => "absent".to_string(),
        Err(err) => format!("error({err:#})"),
    }
}

/// Custom deserializer that accepts every shape we've ever shipped:
///
///   1. Legacy plaintext: `["nvapi-x", "nvapi-y"]`. Each becomes a NIM
///      key with a fresh UUID and empty base_url (so the runtime falls
///      back to the canonical NIM endpoint).
///   2. Pre-multi-provider structured: `[{ "id", "value", "label"?,
///      "expires_at"? }]` without `provider`. We default the provider
///      to NIM so existing configs keep working unchanged.
///   3. Current structured: same as (2) plus `"provider"` and
///      `"base_url"`.
///
/// Plaintext entries get a fresh UUID. Structured entries flow through
/// unchanged save for the implicit `ProviderKind::default()` fill-in
/// done by serde when the field is missing (see `#[serde(default)]`).
fn deserialize_nim_keys<'de, D>(deserializer: D) -> Result<Vec<NimApiKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(items) => {
            let mut keys = Vec::with_capacity(items.len());
            for item in items {
                let key = match item {
                    serde_json::Value::String(s) => NimApiKey::from_value(s),
                    other => {
                        serde_json::from_value::<NimApiKey>(other).map_err(D::Error::custom)?
                    }
                };
                keys.push(key);
            }
            Ok(keys)
        }
        _ => Err(D::Error::custom("nim_api_keys must be an array")),
    }
}

/// Append a single line to the diagnostic log. Best-effort: silently swallows
/// any IO error so logging never breaks the caller's main flow.
fn diag(msg: &str) {
    let Ok(path) = AppConfig::diagnostic_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let line = format!("[{stamp}] {msg}\n");
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default, deserialize_with = "deserialize_nim_keys")]
        nim_api_keys: Vec<NimApiKey>,
    }

    #[test]
    fn deserializes_legacy_plaintext_array() {
        let raw = r#"{ "nim_api_keys": ["nvapi-aaa", "nvapi-bbb"] }"#;
        let parsed: Wrapper = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.nim_api_keys.len(), 2);
        assert_eq!(parsed.nim_api_keys[0].value, "nvapi-aaa");
        assert!(parsed.nim_api_keys[0].label.is_none());
        assert!(parsed.nim_api_keys[0].expires_at.is_none());
        // Legacy entries assume NIM provider so existing configs keep working.
        assert_eq!(parsed.nim_api_keys[0].provider, ProviderKind::Nim);
        assert!(parsed.nim_api_keys[0].base_url.is_empty());
        // Upgraded entries should have been assigned synthetic UUIDs.
        assert!(!parsed.nim_api_keys[0].id.is_empty());
        assert_ne!(parsed.nim_api_keys[0].id, parsed.nim_api_keys[1].id);
    }

    #[test]
    fn deserializes_structured_array() {
        let raw = r#"{ "nim_api_keys": [
            { "id": "k1", "value": "nvapi-aaa", "label": "primary", "expires_at": 4102444800 },
            { "id": "k2", "value": "nvapi-bbb" }
        ] }"#;
        let parsed: Wrapper = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.nim_api_keys[0].id, "k1");
        assert_eq!(parsed.nim_api_keys[0].label.as_deref(), Some("primary"));
        assert_eq!(parsed.nim_api_keys[0].expires_at, Some(4102444800));
        assert_eq!(parsed.nim_api_keys[0].provider, ProviderKind::Nim);
        assert_eq!(parsed.nim_api_keys[1].id, "k2");
        assert!(parsed.nim_api_keys[1].label.is_none());
        assert!(parsed.nim_api_keys[1].expires_at.is_none());
    }

    #[test]
    fn deserializes_mixed_array() {
        let raw = r#"{ "nim_api_keys": [
            "nvapi-legacy",
            { "id": "k2", "value": "nvapi-new", "label": "secondary" }
        ] }"#;
        let parsed: Wrapper = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.nim_api_keys.len(), 2);
        assert_eq!(parsed.nim_api_keys[0].value, "nvapi-legacy");
        assert!(parsed.nim_api_keys[0].label.is_none());
        assert_eq!(parsed.nim_api_keys[1].label.as_deref(), Some("secondary"));
    }

    #[test]
    fn deserializes_multi_provider_array() {
        let raw = r#"{ "nim_api_keys": [
            { "id": "k1", "value": "nvapi-aaa", "provider": "nim" },
            { "id": "k2", "value": "sk-deepseek-xyz", "provider": "openai_compat",
              "base_url": "https://api.deepseek.com" },
            { "id": "k3", "value": "sk-ant-zhipu", "provider": "anthropic_compat",
              "base_url": "https://open.bigmodel.cn/api/anthropic" }
        ] }"#;
        let parsed: Wrapper = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.nim_api_keys.len(), 3);
        assert_eq!(parsed.nim_api_keys[0].provider, ProviderKind::Nim);
        assert_eq!(parsed.nim_api_keys[1].provider, ProviderKind::OpenaiCompat);
        assert_eq!(parsed.nim_api_keys[1].base_url, "https://api.deepseek.com");
        assert_eq!(
            parsed.nim_api_keys[2].provider,
            ProviderKind::AnthropicCompat
        );
    }

    #[test]
    fn effective_base_url_falls_back_to_provider_default() {
        let nim = NimApiKey::from_value("nvapi-x");
        assert_eq!(
            nim.effective_base_url(),
            "https://integrate.api.nvidia.com/v1"
        );
        let mut custom = NimApiKey::from_value("nvapi-x");
        custom.base_url = "https://my.example.com/v1/".to_string();
        assert_eq!(custom.effective_base_url(), "https://my.example.com/v1");
    }

    /// Empty / whitespace-only global `default_model` must be
    /// rejected by `validate_for_save` so `save_config` (the GUI's
    /// IPC entry point) can surface a clear, actionable error
    /// before the bad value reaches `config.json`. Otherwise a
    /// round-trip through disk would silently break every request
    /// that doesn't match a per-key override or a family-specific
    /// mapping.
    #[test]
    fn validate_for_save_rejects_empty_default_model() {
        let mut cfg = AppConfig::default();
        assert!(
            cfg.validate_for_save().is_ok(),
            "default config should validate"
        );

        cfg.model_mapping.default_model = String::new();
        let err = cfg.validate_for_save().unwrap_err().to_string();
        assert!(
            err.contains("默认模型"),
            "error should point to the default-model field, got: {err}"
        );

        cfg.model_mapping.default_model = "   \t\n  ".to_string();
        assert!(
            cfg.validate_for_save().is_err(),
            "whitespace-only default should also be rejected"
        );

        cfg.model_mapping.default_model = "deepseek-ai/deepseek-v4-flash".to_string();
        assert!(cfg.validate_for_save().is_ok());
    }

    /// `is_empty` must consider both the model strings *and* the
    /// `extra_body` slots — otherwise a key whose only override is
    /// an `extra_body` (e.g. "use my own temperature for the default
    /// upstream") would get its mapping object normalised to `None`
    /// on save and silently lose the override.
    #[test]
    fn per_key_mapping_is_empty_considers_extra_body() {
        let blank = PerKeyModelMappingConfig::default();
        assert!(blank.is_empty(), "all-default config should be empty");

        let only_default_model = PerKeyModelMappingConfig {
            default_model: Some("custom".into()),
            ..Default::default()
        };
        assert!(!only_default_model.is_empty());

        let only_extra_body = PerKeyModelMappingConfig {
            default_extra_body: Some(serde_json::json!({ "temperature": 0.7 })),
            ..Default::default()
        };
        assert!(
            !only_extra_body.is_empty(),
            "an extra_body-only override must NOT be normalised away"
        );

        // An empty `{}` extra_body is just a no-op and *should* be
        // treated as unset — we don't want the GUI's "save → reopen"
        // round-trip to leave a permanent empty object in the file.
        let empty_object_extra_body = PerKeyModelMappingConfig {
            default_extra_body: Some(serde_json::json!({})),
            ..Default::default()
        };
        assert!(empty_object_extra_body.is_empty());

        // Whitespace-only model strings still count as empty too.
        let whitespace_only = PerKeyModelMappingConfig {
            default_model: Some("   ".into()),
            opus_model: Some("\t".into()),
            ..Default::default()
        };
        assert!(whitespace_only.is_empty());
    }
}
