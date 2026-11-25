//! Token-aware context budgeting for `agent:ask`.
//!
//! No tokenizer is bundled: the estimate is deliberately conservative
//! (it over-counts), so a prompt that fits the estimate fits the model. When
//! `max_input_tokens` is not configured the legacy character limits apply
//! unchanged. Nothing here keeps prompt or source text: only counts leave
//! this module.

use std::collections::BTreeSet;

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

/// How much a chunk of a file being changed is favored, and one of its tests.
const MODIFIED_BOOST: f32 = 1.25;
const TEST_BOOST: f32 = 1.15;

/// Ranks chunks of the files in `dirty` (uncommitted work), and the tests of
/// those files, a little higher; the order of everything else is kept. A boost,
/// not a jump: a clearly better hit elsewhere still wins.
pub fn prefer_modified(mut hits: Vec<SearchHit>, dirty: &BTreeSet<String>) -> Vec<SearchHit> {
    if dirty.is_empty() {
        return hits;
    }
    let stems: BTreeSet<String> = dirty.iter().filter_map(|path| stem(path)).filter(|s| s.len() > 2).collect();
    for hit in &mut hits {
        if dirty.contains(&hit.path) {
            hit.score *= MODIFIED_BOOST;
        } else if is_test_path(&hit.path) && stem(&hit.path).is_some_and(|s| stems.iter().any(|d| tests_stem(&s, d))) {
            hit.score *= TEST_BOOST;
        }
    }
    // A stable sort keeps the original order of equal scores.
    hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    hits
}

/// The file name without directories or extensions (`src/auth.test.ts` is `auth`).
fn stem(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let stem = name.split('.').next()?;
    (!stem.is_empty()).then(|| stem.to_lowercase())
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.split('/').any(|part| matches!(part, "test" | "tests" | "__tests__" | "spec" | "specs"))
        || lower.rsplit('/').next().is_some_and(|name| {
            name.contains("_test.") || name.contains(".test.") || name.contains(".spec.") || name.starts_with("test_")
        })
}

/// Whether a test file stem names the file stem `target` (`auth_test`, `test_auth` and `auth` in `tests/`).
fn tests_stem(test: &str, target: &str) -> bool {
    test == target
        || test.strip_suffix("_test") == Some(target)
        || test.strip_prefix("test_") == Some(target)
        || test.strip_suffix("_spec") == Some(target)
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

    fn scored(path: &str, score: f32) -> SearchHit {
        SearchHit { score, ..hit(path, 1, 5, path) }
    }

    #[test]
    fn modified_files_and_their_tests_rank_a_little_higher() {
        let dirty: BTreeSet<String> = ["src/auth.rs".to_owned()].into();
        let hits = vec![
            scored("src/billing.rs", 1.0),
            scored("tests/auth.rs", 0.9),
            scored("src/auth.rs", 0.85),
            scored("src/other.rs", 0.8),
        ];
        let paths: Vec<_> = prefer_modified(hits, &dirty).into_iter().map(|h| h.path).collect();
        // 0.85 * 1.25 and 0.9 * 1.15 pass 1.0; the unrelated files keep their order.
        assert_eq!(paths, ["src/auth.rs", "tests/auth.rs", "src/billing.rs", "src/other.rs"]);
    }

    #[test]
    fn a_clearly_better_hit_still_wins_and_nothing_changes_without_changes() {
        let dirty: BTreeSet<String> = ["src/auth.rs".to_owned()].into();
        let hits = vec![scored("src/billing.rs", 2.0), scored("src/auth.rs", 1.0)];
        assert_eq!(prefer_modified(hits.clone(), &dirty)[0].path, "src/billing.rs");
        let untouched = prefer_modified(hits.clone(), &BTreeSet::new());
        assert_eq!(
            untouched.iter().map(|h| &h.path).collect::<Vec<_>>(),
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    #[test]
    fn recognizes_the_tests_of_a_file() {
        assert!(is_test_path("src/auth_test.go") && is_test_path("web/auth.test.ts") && is_test_path("tests/x.rs"));
        assert!(!is_test_path("src/latest.rs"));
        assert!(tests_stem("auth_test", "auth") && tests_stem("test_auth", "auth") && tests_stem("auth", "auth"));
        assert!(!tests_stem("author_test", "auth"));
    }
}
