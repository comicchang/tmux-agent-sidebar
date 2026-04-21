use serde_json::{Value, json};

pub const MAX_NAME_CHARS: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub user: &'a str,
}

#[derive(Debug)]
pub enum LlmError {
    Http(String),
    InvalidJson(String),
    EmptyContent,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(msg) => write!(f, "http: {msg}"),
            Self::InvalidJson(msg) => write!(f, "invalid response json: {msg}"),
            Self::EmptyContent => write!(f, "llm returned empty content"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Serialize an OpenAI-compatible `/v1/chat/completions` request body.
///
/// Constrains `max_tokens` and `temperature` for short, deterministic
/// titles suitable for a narrow sidebar column.
pub fn build_body(req: &ChatRequest<'_>) -> String {
    let body = json!({
        "model": req.model,
        "messages": [
            {"role": "system", "content": req.system},
            {"role": "user", "content": req.user},
        ],
        "max_tokens": 24,
        "temperature": 0.2,
        "stream": false,
    });
    body.to_string()
}

/// Parse an OpenAI-compatible chat-completions response body and return
/// the post-processed title (first whitespace token, quotes stripped,
/// truncated to `MAX_NAME_CHARS` Unicode scalar values).
pub fn parse_response(body: &str) -> Result<String, LlmError> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| LlmError::InvalidJson(e.to_string()))?;
    let content = value
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| LlmError::InvalidJson("missing choices[0].message.content".into()))?;
    let cleaned = post_process(content);
    if cleaned.is_empty() {
        return Err(LlmError::EmptyContent);
    }
    Ok(cleaned)
}

/// Post-process a raw title candidate: strip surrounding whitespace /
/// quotes / backticks, skip common `Name:` / `Title:` preambles that
/// small models like to emit, take the first whitespace-delimited
/// token, and truncate to `MAX_NAME_CHARS` chars. Defensive — the
/// system prompt already asks for a single short word, but small
/// models often ignore that constraint.
pub fn post_process(raw: &str) -> String {
    let trimmed = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '_' | '[' | ']' | '(' | ')'));
    let after_preamble = strip_preamble(trimmed);
    let first_token = after_preamble.split_whitespace().next().unwrap_or("");
    let stripped = first_token
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '.' | ',' | ':' | ';' | '!' | '?'));
    stripped.chars().take(MAX_NAME_CHARS).collect()
}

/// Drop the leading `label:` in replies like `Name: deploy pipeline`
/// or `title - refactor auth` so the first-token rule selects the real
/// title instead of `name` / `title`. Only strips when the first
/// whitespace-delimited token is one of the known preamble words
/// followed by `:` or `-` (with optional whitespace).
fn strip_preamble(text: &str) -> &str {
    let mut chars = text.char_indices();
    let Some((_, first)) = chars.next() else {
        return text;
    };
    if !first.is_alphabetic() {
        return text;
    }
    // Find end of the first alphabetic run.
    let mut word_end = text.len();
    for (idx, ch) in text.char_indices() {
        if !ch.is_alphabetic() {
            word_end = idx;
            break;
        }
    }
    let word = &text[..word_end];
    const PREAMBLE_WORDS: &[&str] = &["name", "title", "session", "label"];
    if !PREAMBLE_WORDS.iter().any(|w| word.eq_ignore_ascii_case(w)) {
        return text;
    }
    let rest = text[word_end..].trim_start();
    let Some(sep_rest) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('-')) else {
        return text;
    };
    sep_rest.trim_start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_body_has_expected_shape() {
        let body = build_body(&ChatRequest {
            model: "llama3.2:3b",
            system: "name this",
            user: "activity...",
        });
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "llama3.2:3b");
        assert_eq!(v["messages"][0]["role"], "system");
        assert_eq!(v["messages"][0]["content"], "name this");
        assert_eq!(v["messages"][1]["role"], "user");
        assert_eq!(v["messages"][1]["content"], "activity...");
        assert_eq!(v["stream"], false);
        assert!(v["max_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn parse_response_extracts_content() {
        let body = r#"{"choices":[{"message":{"content":"refactor"}}]}"#;
        assert_eq!(parse_response(body).unwrap(), "refactor");
    }

    #[test]
    fn parse_response_errors_on_malformed_json() {
        let err = parse_response("not json").unwrap_err();
        assert!(matches!(err, LlmError::InvalidJson(_)));
    }

    #[test]
    fn parse_response_errors_on_missing_content_path() {
        let body = r#"{"choices":[{}]}"#;
        let err = parse_response(body).unwrap_err();
        assert!(matches!(err, LlmError::InvalidJson(_)));
    }

    #[test]
    fn parse_response_errors_on_empty_content() {
        let body = r#"{"choices":[{"message":{"content":"   "}}]}"#;
        let err = parse_response(body).unwrap_err();
        assert!(matches!(err, LlmError::EmptyContent));
    }

    #[test]
    fn post_process_strips_surrounding_quotes() {
        assert_eq!(post_process("\"refactor\""), "refactor");
        assert_eq!(post_process("'build'"), "build");
        assert_eq!(post_process("`docs`"), "docs");
    }

    #[test]
    fn post_process_takes_first_whitespace_token() {
        assert_eq!(post_process("refactor auth module"), "refactor");
        // When no known preamble prefix is present, the first token
        // is the title.
        assert_eq!(post_process("deploy pipeline"), "deploy");
    }

    #[test]
    fn post_process_strips_trailing_punctuation() {
        assert_eq!(post_process("refactor."), "refactor");
        assert_eq!(post_process("refactor!"), "refactor");
        assert_eq!(post_process("\"refactor.\""), "refactor");
    }

    #[test]
    fn post_process_truncates_to_max_chars() {
        let long = "supercalifragilisticexpialidocious";
        let out = post_process(long);
        assert_eq!(out.chars().count(), MAX_NAME_CHARS);
        assert!(long.starts_with(&out));
    }

    #[test]
    fn post_process_preserves_multibyte_chars_within_limit() {
        // 日本語 = 3 chars, well within 16
        assert_eq!(post_process("日本語"), "日本語");
    }

    #[test]
    fn post_process_truncates_by_chars_not_bytes_for_multibyte() {
        // 20 kanji; should keep first MAX_NAME_CHARS graphemes (char-approximation).
        let input = "実装タスク検証リファクタデプロイ確認予約調査編集備考";
        let out = post_process(input);
        assert_eq!(out.chars().count(), MAX_NAME_CHARS);
    }

    #[test]
    fn post_process_empty_or_whitespace_returns_empty() {
        assert_eq!(post_process(""), "");
        assert_eq!(post_process("   \n"), "");
    }

    #[test]
    fn post_process_strips_name_preamble() {
        assert_eq!(post_process("Name: deploy pipeline"), "deploy");
        assert_eq!(post_process("name: refactor"), "refactor");
        assert_eq!(post_process("TITLE: docs rewrite"), "docs");
        assert_eq!(post_process("title - build"), "build");
        assert_eq!(post_process("Session: fix-bug"), "fix-bug");
    }

    #[test]
    fn post_process_does_not_strip_when_word_is_not_a_preamble() {
        // "refactor:" is not a known preamble → kept as-is, trailing
        // colon stripped by the existing punctuation rule.
        assert_eq!(post_process("refactor: auth"), "refactor");
    }

    #[test]
    fn post_process_leaves_preamble_alone_without_separator() {
        // Without `:` or `-`, the preamble word is the title itself.
        assert_eq!(post_process("name"), "name");
        assert_eq!(post_process("Title"), "Title");
    }
}
