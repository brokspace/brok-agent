//! Token-aware context budgeting for `agent:ask`.
//!
//! No tokenizer is bundled: the estimate is deliberately conservative
//! (it over-counts), so a prompt that fits the estimate fits the model. When
//! `max_input_tokens` is not configured the legacy character limits apply
//! unchanged. Nothing here keeps prompt or source text: only counts leave
//! this module.

use brok_sdk::prelude::SearchHit;

/// Characters per token assumed for ASCII text. Code and identifiers
/// tokenize at about 3 to 4 characters per token; 3 errs on the safe side.
const ASCII_CHARS_PER_TOKEN: usize = 3;

/// Tokens a message costs besides its text (role, separators).
pub const MESSAGE_OVERHEAD_TOKENS: usize = 4;

/// Conservative token estimate of `text`: ASCII counts one token per three
/// characters (rounded up), every other character one token (CJK and emoji
/// are often a token or more each).
pub fn estimate_tokens(text: &str) -> usize {
    let (mut ascii, mut other) = (0usize, 0usize);
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(ASCII_CHARS_PER_TOKEN) + other
}

/// A size limit, in the unit the user configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// Characters (legacy behavior).
    Chars(usize),
    /// Estimated tokens.
    Tokens(usize),
}

impl Limit {
    /// What `text` costs against this limit.
    pub fn cost(self, text: &str) -> usize {
        match self {
            Limit::Chars(_) => text.chars().count(),
            Limit::Tokens(_) => estimate_tokens(text),
        }
    }

    /// The limit's amount.
    pub fn amount(self) -> usize {
        match self {
            Limit::Chars(n) | Limit::Tokens(n) => n,
        }
    }

    /// The same unit with another amount.
    pub fn with_amount(self, amount: usize) -> Self {
        match self {
            Limit::Chars(_) => Limit::Chars(amount),
            Limit::Tokens(_) => Limit::Tokens(amount),
        }
    }

    /// Characters that fit in `amount` of this unit.
    pub fn max_chars(self) -> usize {
        match self {
            Limit::Chars(n) => n,
            Limit::Tokens(n) => n.saturating_mul(ASCII_CHARS_PER_TOKEN),
        }
    }
}

/// Estimated token use of one request. Counts only, never text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// System prompt, including skill instructions.
    pub system: usize,
    /// Conversation history kept.
    pub history: usize,
    /// The question, with its context framing.
    pub question: usize,
    /// Retrieved context.
    pub context: usize,
}

impl TokenUsage {
    /// Estimated prompt size.
    pub fn total(&self) -> usize {
        self.system + self.history + self.question + self.context
    }
}

/// Drops hits that repeat what a better ranked hit already provides: the
/// same bytes, or a line range of the same file that overlaps one already
/// kept. `hits` must be ranked best first; the order is preserved.
pub fn deduplicate(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut kept: Vec<SearchHit> = Vec::with_capacity(hits.len());
    for hit in hits {
        let repeated = kept.iter().any(|k| {
            k.path == hit.path
                && (k.content == hit.content || (hit.start_line <= k.end_line && k.start_line <= hit.end_line))
        });
        if !repeated {
            kept.push(hit);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, start: u32, end: u32, content: &str) -> SearchHit {
        SearchHit {
            chunk_id: format!("{path}#{start}"),
            partition: "root".into(),
            path: path.into(),
            language: "rust".into(),
            kind: "function".into(),
            symbol: None,
            signature: None,
            start_line: start,
            end_line: end,
            score: 1.0,
            content: content.into(),
        }
    }

    #[test]
    fn estimates_are_conservative() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens("日本語"), 3);
        // Real tokenizers average 3.5 to 4.5 characters per token on code.
        let code = "fn route(&self, event: Event) -> Result<()> { self.bus.send(event) }";
        assert!(estimate_tokens(code) >= code.len() / 4);
    }

    #[test]
    fn limits_measure_in_their_own_unit() {
        assert_eq!(Limit::Chars(10).cost("日本語"), 3);
        assert_eq!(Limit::Tokens(10).cost("abcdef"), 2);
        assert_eq!(Limit::Tokens(10).max_chars(), 30);
        assert_eq!(Limit::Chars(10).with_amount(4), Limit::Chars(4));
    }

    #[test]
    fn deduplicate_drops_overlaps_and_copies_keeping_rank() {
        let hits = vec![
            hit("a.rs", 10, 30, "x"),
            hit("a.rs", 25, 40, "y"),
            hit("a.rs", 41, 50, "z"),
            hit("b.rs", 10, 30, "x"),
            hit("b.rs", 100, 110, "x"),
        ];
        let kept = deduplicate(hits);
        let ids: Vec<_> = kept.iter().map(|h| h.chunk_id.as_str()).collect();
        assert_eq!(ids, ["a.rs#10", "a.rs#41", "b.rs#10"]);
    }
}
