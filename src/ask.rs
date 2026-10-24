//! Retrieval augmented answers for `agent:ask`.
//!
//! Pipeline: validate prompt → hybrid search through the core index service
//! → assemble and redact context → stream the answer from the LLM back to
//! the requester as `agent:ask.chunk` replies, closed by `agent:ask.done`
//! (or `agent:ask.error`).

use std::pin::pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use brok_sdk::prelude::*;
use futures_util::StreamExt;

use crate::guardrails::{redact_secrets, validate_prompt};
use crate::llm::{LlmClient, Message};
use crate::settings::{AgentSettings, LlmConfig};

const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

/// System prompt used unless `system_prompt` is configured.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are Brok, a senior engineer embedded in the user's \
workspace. Answer precisely using the provided workspace context, cite files as `path:line`, show \
code when it helps, and say so plainly when the context does not contain the answer.";

/// Context ready to be sent to the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssembledContext {
    /// Markdown context block.
    pub text: String,
    /// Chunks that made it into the context.
    pub sources: Vec<SourceRef>,
    /// Secrets redacted from the context.
    pub redactions: usize,
}

/// Renders search hits as Markdown, stopping before `max_chars` characters,
/// and redacts secrets. The first hit is truncated rather than dropped so
/// there is always some context when anything was found.
pub fn assemble_context(hits: &[SearchHit], max_chars: usize) -> AssembledContext {
    let mut context = AssembledContext::default();
    let mut used = 0;
    for hit in hits {
        let symbol = hit.symbol.as_deref().map(|s| format!(" ({s})")).unwrap_or_default();
        let mut block = format!(
            "### {}:{}-{}{symbol}\n```{}\n{}\n```\n\n",
            hit.path,
            hit.start_line,
            hit.end_line,
            fence_language(&hit.language),
            hit.content.trim_end()
        );
        let len = block.chars().count();
        if used + len > max_chars {
            if !context.sources.is_empty() || max_chars == 0 {
                break;
            }
            block = block.chars().take(max_chars).collect();
            block.push_str("\n```\n");
        }
        used += len;
        context.text.push_str(&block);
        context.sources.push(SourceRef {
            path: hit.path.clone(),
            start_line: hit.start_line,
            end_line: hit.end_line,
            symbol: hit.symbol.clone(),
        });
    }
    let (text, redactions) = redact_secrets(&context.text);
    context.text = text;
    context.redactions = redactions;
    context
}

/// The user message combining question and context.
pub fn user_message(prompt: &str, context: &str) -> String {
    if context.is_empty() {
        format!("No indexed workspace context matched this question.\n\n## Question\n{prompt}")
    } else {
        format!("## Workspace context\n\n{context}## Question\n{prompt}")
    }
}

fn fence_language(language: &str) -> &str {
    match language {
        "text" | "unknown" => "",
        other => other,
    }
}

/// Handles one `agent:ask` request, always ending with `done` or `error`.
pub async fn handle(client: BrokClient, request: Event, settings: Arc<AgentSettings>, llm: Arc<LlmConfig>) {
    let outcome = answer(&client, &request, &settings, &llm).await;
    let sent = match outcome {
        Ok(done) => client.reply(&request, &done).await,
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "ask failed");
            client.reply(&request, &AskFailed { message: format!("{e:#}") }).await
        }
    };
    if let Err(e) = sent {
        tracing::warn!(error = %e, "cannot deliver the final ask reply");
    }
}

async fn answer(
    client: &BrokClient,
    request: &Event,
    settings: &AgentSettings,
    llm: &LlmConfig,
) -> anyhow::Result<AskDone> {
    let ask: AskRequest = request.decode().context("malformed agent:ask payload")?;
    validate_prompt(&ask.prompt)?;
    let (provider_name, provider) = llm.resolve(ask.provider.as_deref())?;
    let model = LlmClient::new(provider)?;

    let limit = ask.context_chunks.unwrap_or(settings.context_chunks);
    let hits = if limit == 0 {
        Vec::new()
    } else {
        let search = SearchRequest { query: ask.prompt.clone(), limit, focus: None };
        match client.call::<SearchRequest, SearchResults>(&search, SEARCH_TIMEOUT).await {
            Ok(results) => results.hits,
            Err(e) => {
                tracing::warn!(error = %e, "context search failed; answering without workspace context");
                Vec::new()
            }
        }
    };
    let context = assemble_context(&hits, settings.max_context_chars);
    if context.redactions > 0 {
        tracing::info!(count = context.redactions, "redacted secrets from the context");
    }

    let base = settings.system_prompt.as_deref().unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let system = system_prompt(base, ask.skill.as_ref());
    if let Some(skill) = &ask.skill {
        tracing::info!(skill = %skill.name, "running skill");
    }
    let messages = [Message::system(&system), Message::user(user_message(&ask.prompt, &context.text))];
    let mut deltas = pin!(model.stream(&messages).await?);
    while let Some(delta) = deltas.next().await {
        client.reply(request, &AskChunk { text: delta? }).await?;
    }
    Ok(AskDone { provider: provider_name.to_owned(), model: model.model().to_owned(), sources: context.sources })
}

/// The system prompt, with a skill's instructions appended when the request
/// runs under one (`brok skill run`). Skills are local files the user
/// installed, but their text is still steering, not authority: the base
/// prompt comes first.
fn system_prompt(base: &str, skill: Option<&SkillInvocation>) -> String {
    match skill {
        Some(skill) => format!(
            "{base}\n\nFollow the skill below for this request. The user message is the input to apply it to. \
             If a step needs information or tools you do not have, say what is missing instead of inventing it.\n\n{}",
            skill.instructions.trim()
        ),
        None => base.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_extend_the_system_prompt() {
        assert_eq!(system_prompt("base", None), "base");
        let skill = SkillInvocation { name: "tdd".into(), instructions: "# Skill: tdd\nRed first.".into() };
        let prompt = system_prompt("base", Some(&skill));
        assert!(prompt.starts_with("base\n\n"));
        assert!(prompt.ends_with("Red first."));
    }

    fn hit(path: &str, content: &str) -> SearchHit {
        SearchHit {
            chunk_id: format!("{path}#1"),
            partition: "root".into(),
            path: path.into(),
            language: "rust".into(),
            kind: "function".into(),
            symbol: Some("route".into()),
            signature: None,
            start_line: 10,
            end_line: 20,
            score: 1.0,
            content: content.into(),
        }
    }

    #[test]
    fn renders_hits_with_metadata() {
        let ctx = assemble_context(&[hit("src/bus.rs", "fn route() {}\n")], 10_000);
        assert_eq!(ctx.text, "### src/bus.rs:10-20 (route)\n```rust\nfn route() {}\n```\n\n");
        assert_eq!(ctx.sources.len(), 1);
        assert_eq!(ctx.sources[0].symbol.as_deref(), Some("route"));
    }

    #[test]
    fn respects_the_budget() {
        let hits = vec![hit("a.rs", &"a".repeat(100)), hit("b.rs", &"b".repeat(100))];
        let ctx = assemble_context(&hits, 180);
        assert_eq!(ctx.sources.len(), 1);
        assert!(ctx.text.contains("a.rs") && !ctx.text.contains("b.rs"));
    }

    #[test]
    fn truncates_a_single_oversized_hit() {
        let ctx = assemble_context(&[hit("a.rs", &"x".repeat(1_000))], 100);
        assert_eq!(ctx.sources.len(), 1);
        assert!(ctx.text.chars().count() < 120);
    }

    #[test]
    fn redacts_context() {
        let ctx = assemble_context(&[hit("cfg.rs", "const KEY: &str = \"AKIAABCDEFGHIJKLMNOP\";")], 10_000);
        assert_eq!(ctx.redactions, 1);
        assert!(!ctx.text.contains("AKIA"));
    }

    #[test]
    fn user_message_mentions_missing_context() {
        assert!(user_message("why?", "").starts_with("No indexed workspace context"));
        assert!(user_message("why?", "ctx\n\n").contains("## Workspace context"));
    }
}
