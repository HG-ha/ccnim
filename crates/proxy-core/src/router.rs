use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelMapping {
    pub default_model: String,
    pub default_extra_body: Option<Value>,
    pub opus_model: Option<String>,
    pub opus_extra_body: Option<Value>,
    pub sonnet_model: Option<String>,
    pub sonnet_extra_body: Option<Value>,
    pub haiku_model: Option<String>,
    pub haiku_extra_body: Option<Value>,
}

/// Outcome of a [`ModelMapping::resolve`] call: the upstream model
/// name to send and, optionally, the per-slot `extra_body` JSON
/// object that should be deep-merged into the outgoing request body
/// (config wins over anything the client sent for the same keys).
///
/// The `extra_body` is borrowed from the mapping rather than cloned
/// so resolution stays allocation-free; callers that need to keep
/// it past the mapping's lifetime should `.cloned()` it themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution<'a> {
    pub model: String,
    pub extra_body: Option<&'a Value>,
}

impl ModelMapping {
    /// Resolve the upstream model name to use for an incoming Claude
    /// model. Resolution order:
    ///
    ///   1. Family-specific override (`opus_model` / `sonnet_model` /
    ///      `haiku_model`) when set and non-empty.
    ///   2. The configured `default_model` when non-empty.
    ///   3. The original `claude_model_name`, returned verbatim. This
    ///      is the safety net: emptiness in steps 1 and 2 ought to
    ///      have been caught by `AppConfig::validate_for_save`, but a
    ///      hand-edited `config.json` could still slip through, and
    ///      forwarding `""` to the upstream would deterministically
    ///      hit a 400 the user can't easily diagnose. Passthrough
    ///      keeps the request shape valid; if the upstream doesn't
    ///      accept the Claude name it'll surface its own clear
    ///      "unknown model" error instead.
    ///
    /// All optional / configured values are trimmed before use so
    /// stray whitespace from a manual edit (`"   "`) is treated the
    /// same as "not set".
    ///
    /// The returned [`Resolution`] also carries the slot's
    /// `extra_body` (if any), borrowed from this mapping. The
    /// `extra_body` follows the same slot the model name came from:
    /// if the request matched on `opus_model`, the caller gets
    /// `opus_extra_body`; if it fell through to `default_model`,
    /// `default_extra_body`; on full passthrough (step 3), `None`
    /// because the request is no longer routed by any mapping slot
    /// and applying an arbitrary slot's extras would silently change
    /// the semantics of an already-broken config.
    pub fn resolve<'a>(&'a self, claude_model_name: &str) -> Resolution<'a> {
        let lower = claude_model_name.to_lowercase();
        let pick = |opt: &Option<String>| {
            opt.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        if lower.contains("opus") {
            if let Some(model) = pick(&self.opus_model) {
                return Resolution {
                    model,
                    extra_body: extra_body_object(&self.opus_extra_body),
                };
            }
        }
        if lower.contains("haiku") {
            if let Some(model) = pick(&self.haiku_model) {
                return Resolution {
                    model,
                    extra_body: extra_body_object(&self.haiku_extra_body),
                };
            }
        }
        if lower.contains("sonnet") {
            if let Some(model) = pick(&self.sonnet_model) {
                return Resolution {
                    model,
                    extra_body: extra_body_object(&self.sonnet_extra_body),
                };
            }
        }
        let trimmed_default = self.default_model.trim();
        if !trimmed_default.is_empty() {
            return Resolution {
                model: trimmed_default.to_string(),
                extra_body: extra_body_object(&self.default_extra_body),
            };
        }
        Resolution {
            model: claude_model_name.to_string(),
            extra_body: None,
        }
    }
}

/// Treat anything that isn't a JSON object as "no overrides" — non-
/// object values can't be sensibly merged into a request body and
/// silently dropping them here is preferable to forwarding malformed
/// JSON (which would 400 upstream with a confusing error). The GUI
/// already validates user input as an object, so this guard is
/// only really exercised by hand-edited configs.
fn extra_body_object(extra: &Option<Value>) -> Option<&Value> {
    match extra {
        Some(v @ Value::Object(map)) if !map.is_empty() => Some(v),
        _ => None,
    }
}

/// Recursively merge `src` into `target`, with `src` winning on
/// conflicting leaf keys. The merge is "object-only deep" — when
/// both sides at a given path are JSON objects we recurse into them,
/// but for any other shape (arrays, scalars, mismatched types) the
/// value from `src` replaces whatever was in `target`.
///
/// This is the merge strategy used to apply per-slot `extra_body`
/// overrides to outgoing request bodies. We deliberately do *not*
/// concatenate arrays (it's surprising — most users expect their
/// configured `stop` list to fully replace the client's, not append
/// to it) and we don't do shallow object merge either (that would
/// stomp on nested upstream-specific structures like
/// `chat_template_kwargs.thinking` when the user only intended to
/// set, say, `chat_template_kwargs.reasoning_budget`).
pub fn deep_merge_json(target: &mut Value, src: &Value) {
    match (target, src) {
        (Value::Object(target_map), Value::Object(src_map)) => {
            for (k, v) in src_map {
                match target_map.get_mut(k) {
                    Some(existing) => deep_merge_json(existing, v),
                    None => {
                        target_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        // Any leaf or shape mismatch: src wins wholesale.
        (target, src) => {
            *target = src.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapping_models_only() -> ModelMapping {
        ModelMapping {
            default_model: "deepseek-ai/deepseek-v4-flash".to_string(),
            opus_model: Some("moonshotai/kimi-k2.5".to_string()),
            sonnet_model: Some("z-ai/glm4.7".to_string()),
            haiku_model: None,
            ..Default::default()
        }
    }

    #[test]
    fn resolves_specific_claude_model_families() {
        let mapping = mapping_models_only();
        assert_eq!(
            mapping.resolve("claude-opus-4-20250514").model,
            "moonshotai/kimi-k2.5"
        );
        assert_eq!(
            mapping.resolve("claude-sonnet-4-20250514").model,
            "z-ai/glm4.7"
        );
        assert_eq!(
            mapping.resolve("claude-3-haiku-20240307").model,
            "deepseek-ai/deepseek-v4-flash"
        );
    }

    /// Whitespace-only family overrides count as "not set" — they
    /// can leak in via a hand-edited config.json or a GUI race that
    /// stores `"   "` and we don't want to forward that to the
    /// upstream, which would 400.
    #[test]
    fn whitespace_only_family_override_inherits_default() {
        let mapping = ModelMapping {
            default_model: "default-model".to_string(),
            opus_model: Some("   ".to_string()),
            sonnet_model: Some("\t\n".to_string()),
            haiku_model: Some("".to_string()),
            ..Default::default()
        };
        assert_eq!(mapping.resolve("claude-opus-4").model, "default-model");
        assert_eq!(mapping.resolve("claude-sonnet-4").model, "default-model");
        assert_eq!(mapping.resolve("claude-3-haiku").model, "default-model");
    }

    /// Defensive passthrough: when *both* the family override and the
    /// global default are empty (a hand-edited config.json that
    /// bypassed `AppConfig::validate_for_save`), `resolve` must NOT
    /// return `""` — that would silently 400 the upstream call.
    /// Returning the original Claude model name instead lets the
    /// upstream surface its own "unknown model" error, which is
    /// vastly easier to debug.
    #[test]
    fn empty_default_falls_back_to_passthrough() {
        let mapping = ModelMapping::default();
        assert_eq!(
            mapping.resolve("claude-3-haiku-20240307").model,
            "claude-3-haiku-20240307"
        );
        assert_eq!(mapping.resolve("anything-goes").model, "anything-goes");
    }

    /// And whitespace-only default is treated the same as empty —
    /// a `"   "` slipping through must trigger passthrough rather
    /// than rewriting requests to a literal whitespace string.
    #[test]
    fn whitespace_only_default_falls_back_to_passthrough() {
        let mapping = ModelMapping {
            default_model: "   \t".to_string(),
            ..Default::default()
        };
        assert_eq!(mapping.resolve("claude-sonnet-4").model, "claude-sonnet-4");
    }

    /// Default trimming: a value with leading/trailing whitespace
    /// should still be honoured, just normalised.
    #[test]
    fn trims_whitespace_around_resolved_value() {
        let mapping = ModelMapping {
            default_model: "  default-model  ".to_string(),
            opus_model: Some("  opus-model  ".to_string()),
            ..Default::default()
        };
        assert_eq!(mapping.resolve("claude-opus-4").model, "opus-model");
        assert_eq!(mapping.resolve("claude-3-haiku").model, "default-model");
    }

    /// `extra_body` follows the slot the model name came from. An
    /// Opus request that matches `opus_model` returns
    /// `opus_extra_body`, NOT `default_extra_body`, even when both
    /// are set.
    #[test]
    fn extra_body_follows_resolved_slot() {
        let mapping = ModelMapping {
            default_model: "default-model".to_string(),
            default_extra_body: Some(json!({ "temperature": 0.1 })),
            opus_model: Some("opus-model".to_string()),
            opus_extra_body: Some(json!({ "temperature": 0.9 })),
            sonnet_model: None,
            sonnet_extra_body: Some(json!({ "ignored": true })),
            haiku_model: None,
            haiku_extra_body: None,
        };

        let opus = mapping.resolve("claude-opus-4");
        assert_eq!(opus.model, "opus-model");
        assert_eq!(opus.extra_body, Some(&json!({ "temperature": 0.9 })));

        // Sonnet request falls through to the default slot because
        // sonnet_model is None — and so should pick up
        // `default_extra_body`, NOT `sonnet_extra_body`. (Configuring
        // an extra_body without configuring the matching model name
        // is a user misconfiguration; the safe behaviour is "honour
        // the slot you actually routed through".)
        let sonnet = mapping.resolve("claude-sonnet-4");
        assert_eq!(sonnet.model, "default-model");
        assert_eq!(sonnet.extra_body, Some(&json!({ "temperature": 0.1 })));
    }

    /// Empty / non-object `extra_body` is treated as "no overrides".
    /// This protects upstream from receiving malformed JSON when a
    /// user hand-edits `secrets.json` and accidentally writes a
    /// number, string, or `{}` — the GUI input box would reject
    /// these but the on-disk format is permissive.
    #[test]
    fn extra_body_ignores_non_object_and_empty_object() {
        let mapping = ModelMapping {
            default_model: "default-model".to_string(),
            default_extra_body: Some(json!({})),
            opus_model: Some("opus-model".to_string()),
            opus_extra_body: Some(json!(42)),
            sonnet_model: Some("sonnet-model".to_string()),
            sonnet_extra_body: Some(json!("not-an-object")),
            haiku_model: Some("haiku-model".to_string()),
            haiku_extra_body: Some(json!([1, 2, 3])),
        };
        for claude in [
            "claude-opus",
            "claude-sonnet",
            "claude-haiku",
            "claude-other",
        ] {
            assert_eq!(mapping.resolve(claude).extra_body, None);
        }
    }

    /// Passthrough resolution (both family + default empty) should
    /// NOT carry any extra_body — the request isn't being routed
    /// through any configured slot, so applying an arbitrary slot's
    /// extras would silently re-shape an already-broken request in
    /// ways the user can't trace.
    #[test]
    fn passthrough_resolution_carries_no_extra_body() {
        let mapping = ModelMapping {
            default_extra_body: Some(json!({ "should": "not-leak" })),
            ..Default::default()
        };
        let res = mapping.resolve("claude-3-haiku");
        assert_eq!(res.model, "claude-3-haiku");
        assert_eq!(res.extra_body, None);
    }

    /// Object-deep merge: nested keys merge, scalar conflicts pick
    /// `src`, missing keys get inserted.
    #[test]
    fn deep_merge_json_object_deep() {
        let mut target = json!({
            "temperature": 0.1,
            "chat_template_kwargs": {
                "thinking": true,
                "reasoning_budget": 1024
            },
            "untouched": "stays"
        });
        let src = json!({
            "temperature": 0.9,
            "chat_template_kwargs": {
                "reasoning_budget": 4096,
                "extra_flag": "added"
            },
            "new_top_level": [1, 2]
        });
        deep_merge_json(&mut target, &src);
        assert_eq!(
            target,
            json!({
                "temperature": 0.9,
                "chat_template_kwargs": {
                    "thinking": true,
                    "reasoning_budget": 4096,
                    "extra_flag": "added"
                },
                "untouched": "stays",
                "new_top_level": [1, 2]
            })
        );
    }

    /// Arrays are NOT concatenated — `src` replaces the whole array.
    /// (Most users expect "I configured `stop = [\"END\"]`" to mean
    /// "the upstream sees exactly `[\"END\"]`", not "append END to
    /// whatever the client sent".)
    #[test]
    fn deep_merge_json_arrays_replace_not_concat() {
        let mut target = json!({ "stop": ["A", "B"] });
        let src = json!({ "stop": ["C"] });
        deep_merge_json(&mut target, &src);
        assert_eq!(target, json!({ "stop": ["C"] }));
    }

    /// Type mismatch: `src` wins wholesale (object replaces scalar
    /// and vice versa).
    #[test]
    fn deep_merge_json_type_mismatch_src_wins() {
        let mut target = json!({ "k": "string" });
        deep_merge_json(&mut target, &json!({ "k": { "nested": 1 } }));
        assert_eq!(target, json!({ "k": { "nested": 1 } }));

        let mut target = json!({ "k": { "nested": 1 } });
        deep_merge_json(&mut target, &json!({ "k": "scalar" }));
        assert_eq!(target, json!({ "k": "scalar" }));
    }
}
