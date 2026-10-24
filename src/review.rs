//! Pre-indexation review: selected files are sent to the LLM before the
//! `file:changed` event that triggers indexing is published.

use std::path::PathBuf;

use anyhow::Context;
use brok_sdk::BrokClient;
use brok_sdk::payload::{ReviewResult, ReviewVerdict};
use futures_util::{StreamExt, stream};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::guardrails::redact_secrets;
use crate::llm::{LlmClient, Message};
use crate::settings::{LlmConfig, ReviewSettings};

const MAX_CONCURRENT_REVIEWS: usize = 2;

const REVIEW_PROMPT: &str = "You are a meticulous senior code reviewer. Review the file for bugs, \
security problems and risky changes. Answer ONLY with a JSON object of the form \
{\"verdict\": \"pass\" | \"warn\" | \"fail\", \"summary\": \"<one or two sentences>\"}.";

/// Reviews changed files matching the configured globs.
#[derive(Debug)]
pub struct Reviewer {
    include: GlobSet,
    exclude: GlobSet,
    max_bytes: usize,
    root: PathBuf,
    llm: LlmClient,
}

impl Reviewer {
    /// Builds a reviewer, or `None` when reviews are disabled.
    pub fn new(settings: &ReviewSettings, llm: &LlmConfig, root: PathBuf) -> anyhow::Result<Option<Self>> {
        if !settings.enabled {
            return Ok(None);
        }
        let (_, provider) = llm.resolve(settings.provider.as_deref())?;
        Ok(Some(Self {
            include: glob_set(&settings.include).context("review.include")?,
            exclude: glob_set(&settings.exclude).context("review.exclude")?,
            max_bytes: usize::try_from(settings.max_file_kb.saturating_mul(1024)).unwrap_or(usize::MAX),
            root,
            llm: LlmClient::new(provider)?,
        }))
    }

    /// Whether `path` (workspace relative) is subject to review.
    pub fn selects(&self, path: &str) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }

    /// Reviews every selected path and publishes one [`ReviewResult`] each.
    pub async fn review_all(&self, client: &BrokClient, paths: &[String]) {
        stream::iter(paths.iter().filter(|p| self.selects(p)))
            .for_each_concurrent(MAX_CONCURRENT_REVIEWS, |path| async move {
                match self.review(path).await {
                    Ok(Some(result)) => {
                        tracing::info!(path, verdict = ?result.verdict, "reviewed");
                        if let Err(e) = client.emit(&result).await {
                            tracing::warn!(error = %e, "cannot publish review result");
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(path, error = %e, "review failed"),
                }
            })
            .await;
    }

    async fn review(&self, path: &str) -> anyhow::Result<Option<ReviewResult>> {
        let bytes = match tokio::fs::read(self.root.join(path)).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let head = &bytes[..bytes.len().min(self.max_bytes)];
        if head.contains(&0) {
            return Ok(None); // binary
        }
        let (content, _) = redact_secrets(&String::from_utf8_lossy(head));
        let truncated = if bytes.len() > self.max_bytes { "\n(truncated)" } else { "" };
        let messages =
            [Message::system(REVIEW_PROMPT), Message::user(format!("File: {path}\n```\n{content}\n```{truncated}"))];
        let answer = self.llm.complete(&messages).await?;
        Ok(Some(parse_review(path, &answer)))
    }
}

fn glob_set(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).literal_separator(true).build()?);
    }
    builder.build()
}

#[derive(Deserialize)]
struct RawReview {
    verdict: String,
    #[serde(default)]
    summary: String,
}

/// Interprets the model answer, tolerating code fences and chatter around
/// the JSON object. Unparseable answers become a `warn` with the raw text.
pub fn parse_review(path: &str, answer: &str) -> ReviewResult {
    let parsed = answer
        .find('{')
        .zip(answer.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<RawReview>(&answer[start..=end]).ok());
    let (verdict, summary) = match parsed {
        Some(raw) => {
            let verdict = match raw.verdict.trim().to_ascii_lowercase().as_str() {
                "pass" | "ok" => ReviewVerdict::Pass,
                "fail" | "error" => ReviewVerdict::Fail,
                _ => ReviewVerdict::Warn,
            };
            (verdict, raw.summary.trim().to_owned())
        }
        None => (ReviewVerdict::Warn, answer.trim().chars().take(500).collect()),
    };
    ReviewResult { path: path.to_owned(), verdict, summary }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_json() {
        let r = parse_review("a.rs", r#"{"verdict":"fail","summary":"SQL injection"}"#);
        assert_eq!(r.verdict, ReviewVerdict::Fail);
        assert_eq!(r.summary, "SQL injection");
    }

    #[test]
    fn tolerates_fences_and_chatter() {
        let r = parse_review("a.rs", "Sure!\n```json\n{\"verdict\": \"PASS\", \"summary\": \"fine\"}\n```\nBye");
        assert_eq!(r.verdict, ReviewVerdict::Pass);
        assert_eq!(r.summary, "fine");
    }

    #[test]
    fn falls_back_to_warn() {
        let r = parse_review("a.rs", "looks mostly fine but check the unwrap");
        assert_eq!(r.verdict, ReviewVerdict::Warn);
        assert_eq!(r.summary, "looks mostly fine but check the unwrap");
    }

    #[test]
    fn globs_respect_separators() {
        let include = glob_set(&["src/api/**".into()]).unwrap();
        let exclude = glob_set(&["**/*.md".into()]).unwrap();
        assert!(include.is_match("src/api/v1/users.rs"));
        assert!(!include.is_match("src/apiary.rs"));
        assert!(exclude.is_match("README.md"));
        assert!(exclude.is_match("src/api/README.md"));
    }
}
