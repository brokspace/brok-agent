//! Guardrails applied before anything leaves the machine.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// Longest accepted prompt, in characters.
pub const MAX_PROMPT_CHARS: usize = 16_000;

/// Replacement for redacted secrets.
pub const REDACTED: &str = "[REDACTED]";

/// Why a prompt was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// Nothing to ask.
    #[error("the prompt is empty")]
    Empty,
    /// Too long.
    #[error("the prompt has {0} characters; the limit is {MAX_PROMPT_CHARS}")]
    TooLong(usize),
}

/// Checks that a prompt is non-empty and reasonably sized.
pub fn validate_prompt(prompt: &str) -> Result<(), PromptError> {
    if prompt.trim().is_empty() {
        return Err(PromptError::Empty);
    }
    let len = prompt.chars().count();
    if len > MAX_PROMPT_CHARS {
        return Err(PromptError::TooLong(len));
    }
    Ok(())
}

/// Patterns whose whole match is a secret.
static SECRET_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // PEM private keys (RSA, EC, OPENSSH, ...).
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        // AWS access key ids.
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        // GitHub tokens.
        r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{60,}\b",
        // OpenAI / Anthropic / DeepSeek style keys.
        r"\bsk-[A-Za-z0-9_-]{20,}",
        // Slack tokens.
        r"\bxox[abposr]-[A-Za-z0-9-]{10,}",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid secret pattern"))
    .collect()
});

/// `key = "value"` style assignments; only the value group is redacted.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?P<key>[a-z0-9_.-]*(?:api[_-]?key|secret|password|passwd|token)[a-z0-9_.-]*["']?\s*[:=]\s*)(?P<q>["'])(?P<value>[^"'\n]{8,})["']"#,
    )
    .expect("valid assignment pattern")
});

/// Replaces anything that looks like a credential with [`REDACTED`].
/// Returns the cleaned text and the number of redactions.
pub fn redact_secrets(text: &str) -> (String, usize) {
    let mut count = 0;
    let mut out = text.to_owned();
    for pattern in SECRET_PATTERNS.iter() {
        let replaced = pattern.replace_all(&out, |_: &Captures<'_>| {
            count += 1;
            REDACTED
        });
        out = replaced.into_owned();
    }
    let replaced = ASSIGNMENT.replace_all(&out, |caps: &Captures<'_>| {
        if &caps["value"] == REDACTED {
            return caps[0].to_owned();
        }
        count += 1;
        format!("{}{q}{REDACTED}{q}", &caps["key"], q = &caps["q"])
    });
    (replaced.into_owned(), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_prompts() {
        assert_eq!(validate_prompt("  \n"), Err(PromptError::Empty));
        assert!(validate_prompt("how does the bus route replies?").is_ok());
        assert_eq!(validate_prompt(&"x".repeat(MAX_PROMPT_CHARS + 1)), Err(PromptError::TooLong(16_001)));
    }

    #[test]
    fn redacts_known_token_formats() {
        let text = "aws AKIAABCDEFGHIJKLMNOP gh ghp_abcdefghijklmnopqrstuvwxyz0123456789AB \
                    openai sk-proj-abcdefghijklmnopqrstuvwx slack xoxb-1234567890-abcdef";
        let (out, n) = redact_secrets(text);
        assert_eq!(n, 4, "{out}");
        assert!(!out.contains("AKIA") && !out.contains("ghp_") && !out.contains("sk-proj") && !out.contains("xoxb"));
    }

    #[test]
    fn redacts_private_keys() {
        let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\nafter";
        let (out, n) = redact_secrets(text);
        assert_eq!(n, 1);
        assert_eq!(out, format!("before\n{REDACTED}\nafter"));
    }

    #[test]
    fn redacts_only_assignment_values() {
        let (out, n) = redact_secrets(r#"let api_key = "super-secret-value"; DB_PASSWORD: 'hunter22hunter'"#);
        assert_eq!(n, 2);
        assert_eq!(out, format!(r#"let api_key = "{REDACTED}"; DB_PASSWORD: '{REDACTED}'"#));
    }

    #[test]
    fn leaves_ordinary_code_alone() {
        let code = "fn token_count(text: &str) -> usize { text.len() }\nlet password = read_password();";
        assert_eq!(redact_secrets(code), (code.to_owned(), 0));
    }

    #[test]
    fn does_not_double_count() {
        let (out, n) = redact_secrets(r#"token = "sk-abcdefghijklmnopqrstuvwxyz""#);
        assert_eq!(n, 1);
        assert_eq!(out, format!(r#"token = "{REDACTED}""#));
    }
}
