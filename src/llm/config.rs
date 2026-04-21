use std::collections::HashMap;

pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// Environment variable that carries the bearer token when the LLM
/// endpoint needs one. Kept out of tmux options on purpose: any client
/// that can talk to the tmux server can `tmux show-option` an
/// `@sidebar_llm_*` key, so storing a credential there would make it
/// readable by every tmux session for the user. Env vars inherit only
/// into processes the user launches, which is the trust boundary we
/// want.
pub const API_KEY_ENV: &str = "TMUX_AGENT_SIDEBAR_LLM_API_KEY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmConfig {
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub auto_rename: bool,
    pub timeout_ms: u64,
}

impl LlmConfig {
    /// Returns `None` when the feature is not configured. The minimum
    /// required opt-in is `@sidebar_llm_endpoint` + `@sidebar_llm_model`.
    /// The API key (when needed) is sourced from the
    /// [`API_KEY_ENV`] environment variable, **not** tmux options.
    pub fn from_tmux_options(opts: &HashMap<String, String>) -> Option<Self> {
        Self::from_sources(opts, std::env::var(API_KEY_ENV).ok().as_deref())
    }

    /// Testable seam: same as [`from_tmux_options`] but the API key is
    /// passed in explicitly instead of read from the process
    /// environment.
    pub fn from_sources(opts: &HashMap<String, String>, api_key_env: Option<&str>) -> Option<Self> {
        let endpoint = non_empty_str(opts.get("@sidebar_llm_endpoint").map(String::as_str))?;
        let model = non_empty_str(opts.get("@sidebar_llm_model").map(String::as_str))?;
        let api_key = non_empty_str(api_key_env);
        let auto_rename = read_bool(opts, "@sidebar_llm_auto_rename").unwrap_or(false);
        let timeout_ms = opts
            .get("@sidebar_llm_timeout_ms")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|&ms| ms > 0)
            .unwrap_or(DEFAULT_TIMEOUT_MS);

        Some(Self {
            endpoint,
            model,
            api_key,
            auto_rename,
            timeout_ms,
        })
    }
}

fn non_empty_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn read_bool(opts: &HashMap<String, String>, key: &str) -> Option<bool> {
    let raw = opts.get(key)?.trim().to_ascii_lowercase();
    match raw.as_str() {
        "on" | "true" | "1" => Some(true),
        "off" | "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn build(pairs: &[(&str, &str)], api_key: Option<&str>) -> Option<LlmConfig> {
        LlmConfig::from_sources(&opts(pairs), api_key)
    }

    #[test]
    fn returns_none_when_endpoint_missing() {
        assert!(build(&[("@sidebar_llm_model", "llama3.2")], None).is_none());
    }

    #[test]
    fn returns_none_when_model_missing() {
        assert!(
            build(
                &[(
                    "@sidebar_llm_endpoint",
                    "http://localhost:11434/v1/chat/completions",
                )],
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn returns_none_when_endpoint_is_whitespace() {
        assert!(
            build(
                &[
                    ("@sidebar_llm_endpoint", "   "),
                    ("@sidebar_llm_model", "llama3.2"),
                ],
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn minimum_config_uses_defaults() {
        let cfg = build(
            &[
                (
                    "@sidebar_llm_endpoint",
                    "http://localhost:11434/v1/chat/completions",
                ),
                ("@sidebar_llm_model", "llama3.2:3b"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(cfg.endpoint, "http://localhost:11434/v1/chat/completions");
        assert_eq!(cfg.model, "llama3.2:3b");
        assert!(cfg.api_key.is_none());
        assert!(!cfg.auto_rename);
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn full_config_parses_all_fields_with_api_key_from_env() {
        let cfg = build(
            &[
                (
                    "@sidebar_llm_endpoint",
                    "http://example:8080/v1/chat/completions",
                ),
                ("@sidebar_llm_model", "gpt-4o-mini"),
                ("@sidebar_llm_auto_rename", "on"),
                ("@sidebar_llm_timeout_ms", "5000"),
            ],
            Some("sk-123"),
        )
        .unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("sk-123"));
        assert!(cfg.auto_rename);
        assert_eq!(cfg.timeout_ms, 5_000);
    }

    #[test]
    fn api_key_ignored_if_set_via_tmux_option() {
        // Explicit regression guard: we used to read
        // `@sidebar_llm_api_key` from tmux options. Tmux options are
        // world-readable to any client of the tmux server, so
        // credentials must not live there. Any value set via that key
        // is silently dropped.
        let cfg = build(
            &[
                (
                    "@sidebar_llm_endpoint",
                    "http://example:8080/v1/chat/completions",
                ),
                ("@sidebar_llm_model", "m"),
                ("@sidebar_llm_api_key", "sk-should-be-ignored"),
            ],
            None,
        )
        .unwrap();
        assert!(
            cfg.api_key.is_none(),
            "tmux option @sidebar_llm_api_key must be ignored"
        );
    }

    #[test]
    fn empty_env_api_key_is_treated_as_none() {
        let cfg = build(
            &[
                ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                ("@sidebar_llm_model", "m"),
            ],
            Some("   "),
        )
        .unwrap();
        assert!(cfg.api_key.is_none());
    }

    #[test]
    fn auto_rename_accepts_multiple_truthy_values() {
        for truthy in ["on", "true", "1", "ON", "True"] {
            let cfg = build(
                &[
                    ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                    ("@sidebar_llm_model", "m"),
                    ("@sidebar_llm_auto_rename", truthy),
                ],
                None,
            )
            .unwrap();
            assert!(cfg.auto_rename, "expected {truthy:?} to parse as true");
        }
    }

    #[test]
    fn auto_rename_accepts_multiple_falsy_values() {
        for falsy in ["off", "false", "0", "OFF"] {
            let cfg = build(
                &[
                    ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                    ("@sidebar_llm_model", "m"),
                    ("@sidebar_llm_auto_rename", falsy),
                ],
                None,
            )
            .unwrap();
            assert!(!cfg.auto_rename, "expected {falsy:?} to parse as false");
        }
    }

    #[test]
    fn auto_rename_garbage_is_ignored_and_defaults_false() {
        let cfg = build(
            &[
                ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                ("@sidebar_llm_model", "m"),
                ("@sidebar_llm_auto_rename", "maybe"),
            ],
            None,
        )
        .unwrap();
        assert!(!cfg.auto_rename);
    }

    #[test]
    fn timeout_zero_or_garbage_falls_back_to_default() {
        let cfg = build(
            &[
                ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                ("@sidebar_llm_model", "m"),
                ("@sidebar_llm_timeout_ms", "0"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);

        let cfg = build(
            &[
                ("@sidebar_llm_endpoint", "http://x/v1/chat/completions"),
                ("@sidebar_llm_model", "m"),
                ("@sidebar_llm_timeout_ms", "abc"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
    }
}
