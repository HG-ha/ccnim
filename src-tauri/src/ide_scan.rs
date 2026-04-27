//! Scan and patch user-level `settings.json` files for VSCode-family editors so
//! the official Anthropic *Claude Code* extension picks up our local proxy.
//!
//! ## Rationale
//!
//! VSCode, Cursor, Windsurf, VSCodium and Trae all share the exact same
//! per-user settings layout (`<config_dir>/<vendor>/User/settings.json`) and
//! all consume the `claudeCode.environmentVariables` key from the official
//! `Anthropic.claude-code` extension. Writing a 2-element array under that key
//! is enough to redirect the extension to our local proxy — no per-IDE
//! plugin glue, no DLLs, just a JSON edit.
//!
//! ## Why a hand-rolled JSONC stripper
//!
//! `settings.json` is JSONC — JSON with `//` and `/* */` comments and trailing
//! commas. `serde_json` rejects both. Pulling in a JSONC parser dependency just
//! to set one key is overkill, so we strip comments / trailing commas via a
//! 100-line state machine (the only correctness-critical part is *not*
//! treating `//` inside string literals as a comment, which is unit-tested
//! below). On write we re-serialize as standard JSON, which means inline
//! comments are lost — that's why we always copy the original to
//! `settings.json.bak` first and report it back to the UI.

use std::path::Path;

use directories::BaseDirs;
use serde::Serialize;

/// Static registry of supported VSCode-family editors. The third element is
/// the directory name under the platform-appropriate user config dir
/// (`%APPDATA%` / `~/Library/Application Support` / `~/.config`).
const KNOWN_IDES: &[(&str, &str, &str)] = &[
    ("vscode", "Visual Studio Code", "Code"),
    ("vscode-insiders", "VSCode Insiders", "Code - Insiders"),
    ("cursor", "Cursor", "Cursor"),
    ("windsurf", "Windsurf", "Windsurf"),
    ("vscodium", "VSCodium", "VSCodium"),
    ("trae", "Trae", "Trae"),
];

const ENV_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
const SETTINGS_KEY: &str = "claudeCode.environmentVariables";

#[derive(Debug, Clone, Serialize)]
pub struct IdeProfile {
    pub id: String,
    pub name: String,
    /// Absolute path the app would read/write. Always populated even if
    /// the file does not exist yet — clicking "apply" will create it.
    pub settings_path: String,
    /// Whether `settings.json` actually exists on disk right now.
    pub exists: bool,
    /// Currently configured value of `ANTHROPIC_BASE_URL` inside
    /// `claudeCode.environmentVariables`, if the key is present.
    pub configured_base_url: Option<String>,
    /// Currently configured value of `ANTHROPIC_AUTH_TOKEN` inside
    /// `claudeCode.environmentVariables`, if the key is present.
    pub configured_auth_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct IdeApplyReport {
    pub id: String,
    pub name: String,
    pub settings_path: String,
    /// Path of the `.bak` copy we wrote before mutating, if any.
    pub backup_path: Option<String>,
    /// True iff the original file contained `//` or `/* */` comments that
    /// we stripped during the round-trip. Surface this to the user — they
    /// can recover from `.bak` if they cared about those comments.
    pub comments_stripped: bool,
    /// True if the file was created from scratch.
    pub created: bool,
}

/// Locate every IDE we know about and report whether its `settings.json`
/// already targets our proxy. Best-effort — IDEs that aren't installed
/// simply have `exists: false` and "configured_*" left empty.
pub fn scan_ides() -> Vec<IdeProfile> {
    let Some(base) = BaseDirs::new() else {
        return Vec::new();
    };
    let config_root = base.config_dir().to_path_buf();

    KNOWN_IDES
        .iter()
        .map(|(id, name, dir)| build_profile(id, name, &config_root.join(dir)))
        .collect()
}

fn build_profile(id: &str, name: &str, vendor_dir: &Path) -> IdeProfile {
    let settings_path = vendor_dir.join("User").join("settings.json");
    let exists = settings_path.is_file();

    // A malformed settings.json shouldn't kill the scan — surface it as
    // "not yet configured" and let `apply_settings` produce the actual
    // parse error when the user tries to write.
    let (configured_base_url, configured_auth_token) = if exists {
        read_existing_env(&settings_path).unwrap_or_default()
    } else {
        (None, None)
    };

    IdeProfile {
        id: id.to_string(),
        name: name.to_string(),
        settings_path: settings_path.display().to_string(),
        exists,
        configured_base_url,
        configured_auth_token,
    }
}

fn read_existing_env(path: &Path) -> Result<(Option<String>, Option<String>), String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read failed: {e}"))?;
    if raw.trim().is_empty() {
        return Ok((None, None));
    }
    let (stripped, _) = strip_jsonc(&raw);
    let value: serde_json::Value =
        serde_json::from_str(&stripped).map_err(|e| format!("parse failed: {e}"))?;
    Ok(extract_env_pair(&value))
}

fn extract_env_pair(root: &serde_json::Value) -> (Option<String>, Option<String>) {
    let arr = root
        .as_object()
        .and_then(|o| o.get(SETTINGS_KEY))
        .and_then(|v| v.as_array());
    let Some(arr) = arr else {
        return (None, None);
    };
    let mut base = None;
    let mut token = None;
    for item in arr {
        let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(value) = item.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        match name {
            ENV_BASE_URL => base = Some(value.to_string()),
            ENV_AUTH_TOKEN => token = Some(value.to_string()),
            _ => {}
        }
    }
    (base, token)
}

/// Look up `ide_id` in the registry and rewrite its `settings.json`'s
/// `claudeCode.environmentVariables` key to point at the supplied proxy
/// URL/token. Preserves all other top-level keys; ALWAYS backs up first;
/// re-emits as standard JSON (comments are lost — see module docstring).
pub fn apply_settings(
    ide_id: &str,
    base_url: &str,
    auth_token: &str,
) -> Result<IdeApplyReport, String> {
    let (id, name, dir) = KNOWN_IDES
        .iter()
        .find(|(k, _, _)| *k == ide_id)
        .ok_or_else(|| format!("未知的 IDE id: {ide_id}"))?;

    let base = BaseDirs::new().ok_or_else(|| "无法解析用户配置目录".to_string())?;
    let settings_path = base
        .config_dir()
        .join(dir)
        .join("User")
        .join("settings.json");

    let (raw, created) = if settings_path.exists() {
        let text = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("读取 {} 失败: {e}", settings_path.display()))?;
        (text, false)
    } else {
        (String::new(), true)
    };

    let trimmed = raw.trim();
    let (mut value, comments_stripped) = if trimmed.is_empty() {
        (serde_json::json!({}), false)
    } else {
        let (stripped, had_comments) = strip_jsonc(&raw);
        let parsed: serde_json::Value = serde_json::from_str(&stripped).map_err(|e| {
            format!(
                "解析 {} 失败: {e}\n该文件可能含有非标准 JSONC 语法，请手动处理后重试。",
                settings_path.display()
            )
        })?;
        (parsed, had_comments)
    };

    let obj = value.as_object_mut().ok_or_else(|| {
        format!(
            "{} 顶层不是 JSON 对象，无法注入键。",
            settings_path.display()
        )
    })?;

    obj.insert(
        SETTINGS_KEY.to_string(),
        serde_json::json!([
            { "name": ENV_BASE_URL,   "value": base_url },
            { "name": ENV_AUTH_TOKEN, "value": auth_token },
        ]),
    );

    let backup_path = if !created {
        let backup = settings_path.with_extension("json.bak");
        std::fs::copy(&settings_path, &backup)
            .map_err(|e| format!("备份到 {} 失败: {e}", backup.display()))?;
        Some(backup.display().to_string())
    } else {
        None
    };

    let pretty = serde_json::to_string_pretty(&value).map_err(|e| format!("序列化失败: {e}"))?;

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("创建目录 {} 失败: {e}", parent.display()))?;
    }

    std::fs::write(&settings_path, pretty)
        .map_err(|e| format!("写入 {} 失败: {e}", settings_path.display()))?;

    Ok(IdeApplyReport {
        id: id.to_string(),
        name: name.to_string(),
        settings_path: settings_path.display().to_string(),
        backup_path,
        comments_stripped,
        created,
    })
}

/// Strip `//`/`/* */` comments out of a JSONC string and remove trailing
/// commas before `}` / `]`. Returns `(stripped, had_comments)`. Comments
/// inside string literals are left untouched (this matters for URLs like
/// `https://...` that would otherwise eat half the file).
fn strip_jsonc(input: &str) -> (String, bool) {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut had_comments = false;

    while i < n {
        let c = chars[i];

        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }

        if c == '/' && i + 1 < n {
            match chars[i + 1] {
                '/' => {
                    had_comments = true;
                    i += 2;
                    while i < n && chars[i] != '\n' {
                        i += 1;
                    }
                    // Preserve the newline so line/column counts in the
                    // re-serialized output stay roughly aligned for any
                    // post-mortem reviewer.
                    if i < n {
                        out.push('\n');
                        i += 1;
                    }
                    continue;
                }
                '*' => {
                    had_comments = true;
                    i += 2;
                    while i + 1 < n && !(chars[i] == '*' && chars[i + 1] == '/') {
                        i += 1;
                    }
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }

        out.push(c);
        i += 1;
    }

    (strip_trailing_commas(&out), had_comments)
}

fn strip_trailing_commas(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;

    while i < n {
        let c = chars[i];

        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }

        if c == ',' {
            // Look ahead past whitespace; if the next non-WS char is a
            // closing bracket, the comma is trailing — drop it.
            let mut j = i + 1;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            if j < n && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }

        out.push(c);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_line_comments_outside_strings() {
        let src = r#"{
    // hello
    "url": "https://example.com", // trailing comment
    "n": 1
}"#;
        let (out, had) = strip_jsonc(src);
        assert!(had);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://example.com");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn strips_block_comments() {
        let src = r#"{ /* outer */ "k": /* inner */ "v" }"#;
        let (out, had) = strip_jsonc(src);
        assert!(had);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["k"], "v");
    }

    #[test]
    fn keeps_double_slashes_inside_string_literals() {
        // Critical correctness case: VSCode users *will* have URLs in
        // their settings.json. If we naively cut at the first '//' we
        // would corrupt the file.
        let src = r#"{ "url": "https://example.com/path", "k": 1 }"#;
        let (out, had) = strip_jsonc(src);
        assert!(!had);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["url"], "https://example.com/path");
        assert_eq!(v["k"], 1);
    }

    #[test]
    fn drops_trailing_commas_only() {
        let src = r#"{ "a": [1, 2, 3,], "b": { "x": 1, }, }"#;
        let (out, _) = strip_jsonc(src);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"][2], 3);
        assert_eq!(v["b"]["x"], 1);
    }

    #[test]
    fn extracts_existing_env_pair() {
        let v: serde_json::Value = serde_json::json!({
            "claudeCode.environmentVariables": [
                { "name": "ANTHROPIC_BASE_URL", "value": "http://127.0.0.1:8082" },
                { "name": "ANTHROPIC_AUTH_TOKEN", "value": "freecc" },
                { "name": "OTHER", "value": "ignored" }
            ],
            "unrelated": true
        });
        let (base, token) = extract_env_pair(&v);
        assert_eq!(base.as_deref(), Some("http://127.0.0.1:8082"));
        assert_eq!(token.as_deref(), Some("freecc"));
    }

    #[test]
    fn extracts_returns_none_when_key_missing() {
        let v: serde_json::Value = serde_json::json!({ "foo": "bar" });
        let (base, token) = extract_env_pair(&v);
        assert!(base.is_none());
        assert!(token.is_none());
    }
}
