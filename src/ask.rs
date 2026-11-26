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

use crate::context::{Limit, MESSAGE_OVERHEAD_TOKENS, TokenUsage, deduplicate, estimate_tokens, prefer_modified};
use crate::guardrails::{redact_secrets, validate_prompt};
use crate::llm::{LlmClient, Message, explain};
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

/// Renders search hits as Markdown within `limit`, and redacts secrets.
/// A hit that does not fit is skipped and smaller, lower ranked ones still
/// get their chance; the first hit is truncated rather than dropped so there
/// is always some context when anything was found. Output follows rank order.
pub fn assemble_limited(hits: &[SearchHit], limit: Limit) -> AssembledContext {
    let mut context = AssembledContext::default();
    let mut used = 0;
    let max = limit.amount();
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
        let mut cost = limit.cost(&block);
        if used + cost > max {
            if max == 0 {
                break;
            }
            if !context.sources.is_empty() {
                continue;
            }
            block = block.chars().take(limit.with_amount(max).max_chars()).collect();
            block.push_str("\n```\n");
            cost = limit.cost(&block).min(max);
        }
        used += cost;
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

/// [`assemble_limited`] with a character limit.
#[cfg(test)]
fn assemble_context(hits: &[SearchHit], max_chars: usize) -> AssembledContext {
    assemble_limited(hits, Limit::Chars(max_chars))
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

/// The index search that gathers context for `ask`: this workspace, and its
/// Meta Workspace members when there are some to look at.
fn retrieval_request(ask: &AskRequest, limit: usize, members: bool) -> SearchRequest {
    let scope = if members { SearchScope::Workspace } else { SearchScope::Repository };
    SearchRequest::new(retrieval_query(&ask.prompt, &ask.history)).limit(limit).scope(scope)
}

/// The files with uncommitted changes, kept current by the change tracker.
pub type Dirty = std::sync::Arc<std::sync::RwLock<std::collections::BTreeSet<String>>>;

/// Handles one `agent:ask` request, always ending with `done` or `error`.
pub async fn handle(
    client: BrokClient,
    request: Event,
    settings: Arc<AgentSettings>,
    llm: Arc<LlmConfig>,
    members: bool,
    dirty: Dirty,
) {
    let outcome = answer(&client, &request, &settings, &llm, members, &dirty).await;
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

/// Tells the requester the agent cannot take the question now.
pub async fn refuse_busy(client: BrokClient, request: Event) {
    let message = "brok-agent is busy answering other questions; try again in a moment".to_owned();
    if let Err(e) = client.reply(&request, &AskFailed { message }).await {
        tracing::warn!(error = %e, "cannot deliver the busy reply");
    }
}

async fn answer(
    client: &BrokClient,
    request: &Event,
    settings: &AgentSettings,
    llm: &LlmConfig,
    members: bool,
    dirty: &Dirty,
) -> anyhow::Result<AskDone> {
    let ask: AskRequest = request.decode().context("malformed agent:ask payload")?;
    validate_prompt(&ask.prompt)?;
    let (provider_name, provider) = llm.resolve(ask.provider.as_deref())?;
    let mut provider = provider.clone();
    let tuning = &settings.context;
    if tuning.max_input_tokens.is_some() && provider.max_tokens.is_none() {
        provider.max_tokens = tuning.reserved_output_tokens;
    }
    let explained = |e| anyhow::anyhow!(explain(&e, provider_name, &provider));
    let model = LlmClient::new(&provider).map_err(explained)?;

    let base = settings.system_prompt.as_deref().unwrap_or(DEFAULT_SYSTEM_PROMPT);
    let system = system_prompt(base, ask.skill.as_ref());
    if let Some(skill) = &ask.skill {
        tracing::info!(skill = %skill.name, "running skill");
    }
    let budget = Budget::plan(settings, &system, &ask);

    let limit = ask.context_chunks.unwrap_or(settings.context_chunks);
    let candidates = if budget.adaptive { (limit * 2).min(64) } else { limit };
    let mut hits = if candidates == 0 {
        Vec::new()
    } else {
        let search = retrieval_request(&ask, candidates, members);
        match client.call::<SearchRequest, SearchResults>(&search, SEARCH_TIMEOUT).await {
            Ok(results) => results.hits,
            Err(e) => {
                tracing::warn!(error = %e, "context search failed; answering without workspace context");
                Vec::new()
            }
        }
    };
    if budget.adaptive && tuning.prefer_modified {
        let dirty = dirty.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        hits = prefer_modified(hits, &dirty);
    }
    if tuning.deduplicate {
        hits = deduplicate(hits);
    }
    let context = assemble_limited(&hits, budget.context);
    if context.redactions > 0 {
        tracing::info!(count = context.redactions, "redacted secrets from the context");
    }

    let history = history_messages(&ask.history, budget.history);
    let usage = budget.usage_of(&system, &history, &ask.prompt, &context.text);
    tracing::info!(
        estimated_prompt_tokens = usage.total(),
        system = usage.system,
        history = usage.history,
        question = usage.question,
        context = usage.context,
        budget = budget.max_input_tokens,
        sources = context.sources.len(),
        "prompt assembled"
    );
    let mut messages = vec![Message::system(&system)];
    messages.extend(history);
    messages.push(Message::user(user_message(&ask.prompt, &context.text)));
    let mut deltas = pin!(model.stream(&messages).await.map_err(explained)?);
    while let Some(delta) = deltas.next().await {
        client.reply(request, &AskChunk { text: delta? }).await?;
    }
    Ok(AskDone { provider: provider_name.to_owned(), model: model.model().to_owned(), sources: context.sources })
}

/// Earlier turns kept for a follow-up question.
const MAX_HISTORY_TURNS: usize = 12;
/// Character budget for earlier turns when no token budget is configured.
const MAX_HISTORY_CHARS: usize = 12_000;
/// Prompts shorter than this are treated as follow-ups for retrieval.
const FOLLOW_UP_CHARS: usize = 40;
/// Tokens of the fixed framing around the context and the question.
const FRAMING_TOKENS: usize = 16;

/// How the prompt is split between its parts.
#[derive(Debug, Clone, Copy)]
struct Budget {
    /// Limit of the conversation history.
    history: Limit,
    /// Limit of the retrieved context.
    context: Limit,
    /// Whole-prompt budget, when one is configured.
    max_input_tokens: Option<usize>,
    /// Retrieve extra candidates and keep the best that fit.
    adaptive: bool,
}

impl Budget {
    /// Without `max_input_tokens` the legacy character limits apply. With it,
    /// the fixed parts (system prompt, skill, question) are paid first, then
    /// history up to `max_history_tokens`, and retrieved context gets the rest.
    fn plan(settings: &AgentSettings, system: &str, ask: &AskRequest) -> Self {
        let tuning = &settings.context;
        let Some(max) = tuning.max_input_tokens else {
            return Self {
                history: Limit::Chars(MAX_HISTORY_CHARS),
                context: Limit::Chars(settings.max_context_chars),
                max_input_tokens: None,
                adaptive: false,
            };
        };
        let fixed = estimate_message(system) + estimate_message(&ask.prompt) + FRAMING_TOKENS;
        let remaining = max.saturating_sub(fixed);
        let history = Limit::Tokens(tuning.max_history_tokens.min(remaining));
        let kept: usize = history_messages(&ask.history, history).iter().map(|m| estimate_message(&m.content)).sum();
        Self {
            history,
            context: Limit::Tokens(remaining.saturating_sub(kept)),
            max_input_tokens: Some(max),
            adaptive: tuning.adaptive_retrieval,
        }
    }

    fn usage_of(&self, system: &str, history: &[Message], prompt: &str, context: &str) -> TokenUsage {
        TokenUsage {
            system: estimate_message(system),
            history: history.iter().map(|m| estimate_message(&m.content)).sum(),
            question: estimate_message(prompt) + FRAMING_TOKENS,
            context: estimate_tokens(context),
        }
    }
}

fn estimate_message(text: &str) -> usize {
    estimate_tokens(text) + MESSAGE_OVERHEAD_TOKENS
}

/// Ends a message that was cut to fit the history limit.
const SHORTENED: &str = "\n[shortened]";
/// Stands in for an earlier answer that does not fit.
const OMITTED: &str = "[earlier answer omitted]";

/// The conversation so far as chat messages, oldest first, within the turn
/// count and `limit`.
///
/// What users decide is mostly in their short questions, and long answers
/// should not push it out. In order: the newest question, then earlier
/// questions, then the newest answer (shortened if needed, never below half
/// the limit for lack of earlier questions), then earlier answers that still
/// fit. Roles keep alternating: an answer left out is replaced by a note, and
/// an answer is never sent without its question.
fn history_messages(history: &[ChatTurn], limit: Limit) -> Vec<Message> {
    let mut exchanges: Vec<(&str, Option<String>)> = Vec::new();
    for turn in &history[history.len().saturating_sub(MAX_HISTORY_TURNS)..] {
        match (turn.role, exchanges.last_mut()) {
            (ChatRole::User, _) => exchanges.push((&turn.content, None)),
            (ChatRole::Assistant, Some((_, Some(answer)))) => {
                answer.push_str("\n\n");
                answer.push_str(&turn.content);
            }
            (ChatRole::Assistant, Some((_, answer))) => *answer = Some(turn.content.clone()),
            (ChatRole::Assistant, None) => {} // its question is no longer here
        }
    }
    let Some(((question, answer), earlier)) = exchanges.split_last() else { return Vec::new() };

    let max = limit.amount();
    let reserved = answer.as_deref().map_or(0, |a| limit.cost(a).min(max / 2));
    let Some(question) = shorten(question, limit, max - reserved) else { return Vec::new() };
    let mut used = limit.cost(&question) + reserved;

    // Earlier questions, newest first, each with at least a note as answer.
    let mut kept: Vec<Option<(&str, Option<&str>)>> = vec![None; earlier.len()];
    for (slot, (question, answer)) in kept.iter_mut().zip(earlier).rev() {
        let note = answer.as_deref().map(|a| if limit.cost(a) <= limit.cost(OMITTED) { a } else { OMITTED });
        let cost = limit.cost(question) + note.map_or(0, |n| limit.cost(n));
        if used + cost <= max {
            used += cost;
            *slot = Some((question, note));
        }
    }

    let answer = match answer {
        Some(answer) => {
            let Some(answer) = shorten(answer, limit, max - (used - reserved)) else { return Vec::new() };
            used = used - reserved + limit.cost(&answer);
            Some(answer)
        }
        None => None,
    };

    // Earlier answers in full where they still fit.
    for (slot, (_, full)) in kept.iter_mut().zip(earlier).rev() {
        let (Some((_, Some(note))), Some(full)) = (slot, full.as_deref()) else { continue };
        let extra = limit.cost(full).saturating_sub(limit.cost(note));
        if used + extra <= max {
            used += extra;
            *note = full;
        }
    }

    let mut messages = Vec::new();
    for (question, answer) in kept.into_iter().flatten() {
        messages.push(Message::user(question));
        messages.extend(answer.map(Message::assistant));
    }
    messages.push(Message::user(&question));
    messages.extend(answer.as_deref().map(Message::assistant));
    messages
}

/// `text` if it costs at most `room`, else its longest beginning that does
/// together with the [`SHORTENED`] note. `None` when not even that fits.
fn shorten(text: &str, limit: Limit, room: usize) -> Option<String> {
    if room == 0 {
        return None;
    }
    if limit.cost(text) <= room {
        return Some(text.to_owned());
    }
    let ends: Vec<usize> = text.char_indices().map(|(i, c)| i + c.len_utf8()).collect();
    let cut = |chars: usize| format!("{}{SHORTENED}", &text[..ends[chars - 1]]);
    // The cost grows with the length: find the longest beginning that fits.
    let (mut fits, mut too_long) = (0, ends.len() + 1);
    while too_long - fits > 1 {
        let middle = usize::midpoint(fits, too_long);
        if limit.cost(&cut(middle)) <= room {
            fits = middle;
        } else {
            too_long = middle;
        }
    }
    (fits > 0).then(|| cut(fits))
}

/// What to search the index for. A short follow-up ("and where is it
/// tested?") carries little meaning alone, so the last user question is
/// prepended.
fn retrieval_query(prompt: &str, history: &[ChatTurn]) -> String {
    let last_question = history.iter().rev().find(|t| t.role == ChatRole::User);
    match last_question {
        Some(turn) if prompt.chars().count() < FOLLOW_UP_CHARS => format!("{} {prompt}", turn.content),
        _ => prompt.to_owned(),
    }
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

    fn turn(role: ChatRole, content: &str) -> ChatTurn {
        ChatTurn { role, content: content.into() }
    }

    #[test]
    fn retrieval_widens_to_the_members_only_when_there_are_some() {
        let ask: AskRequest = serde_json::from_str(r#"{"prompt":"where is the invoice total computed?"}"#).unwrap();
        assert_eq!(retrieval_request(&ask, 8, false).scope, SearchScope::Repository);
        let wide = retrieval_request(&ask, 16, true);
        assert_eq!((wide.scope, wide.limit), (SearchScope::Workspace, 16));
    }

    #[test]
    fn history_keeps_the_newest_turns_in_order() {
        let history: Vec<ChatTurn> = (0..20)
            .map(|i| turn(if i % 2 == 0 { ChatRole::User } else { ChatRole::Assistant }, &format!("m{i}")))
            .collect();
        let messages = history_messages(&history, Limit::Chars(MAX_HISTORY_CHARS));
        assert_eq!(messages.len(), MAX_HISTORY_TURNS);
        assert_eq!(messages.first().unwrap().content, "m8");
        assert_eq!(messages.last().unwrap(), &Message::assistant("m19"));
        assert_eq!(messages[0].role, crate::llm::Role::User);
    }

    #[test]
    fn history_respects_the_character_budget() {
        let big = "x".repeat(MAX_HISTORY_CHARS - 10);
        let history = vec![turn(ChatRole::User, &big), turn(ChatRole::Assistant, "short answer")];
        // A huge question is cut to leave room for its short answer.
        let messages = history_messages(&history, Limit::Chars(1_000));
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.ends_with(SHORTENED));
        assert_eq!(messages[1], Message::assistant("short answer"));
        assert_eq!(messages.iter().map(|m| m.content.chars().count()).sum::<usize>(), 1_000);
        assert!(history_messages(&[], Limit::Chars(1)).is_empty());
        assert!(history_messages(&history, Limit::Chars(0)).is_empty(), "history turned off");
    }

    #[test]
    fn a_long_last_answer_is_shortened_instead_of_dropping_the_history() {
        let history = vec![
            turn(ChatRole::User, "we keep the retry limit at 3"),
            turn(ChatRole::Assistant, &"b".repeat(2_000)),
            turn(ChatRole::User, "show the client"),
            turn(ChatRole::Assistant, &"a".repeat(9_000)),
        ];
        let messages = history_messages(&history, Limit::Tokens(1_200));
        assert_eq!(messages[0], Message::user("we keep the retry limit at 3"));
        assert_eq!(messages[1], Message::assistant(OMITTED));
        assert_eq!(messages[2], Message::user("show the client"));
        assert!(messages[3].content.starts_with("aaa") && messages[3].content.ends_with(SHORTENED));
        assert!(estimate_tokens(&messages[3].content) > 1_100, "the newest answer gets the room left");
    }

    #[test]
    fn earlier_answers_return_when_there_is_room_for_them() {
        let history = vec![
            turn(ChatRole::User, "one"),
            turn(ChatRole::Assistant, &"x".repeat(500)),
            turn(ChatRole::User, "two"),
            turn(ChatRole::Assistant, &"y".repeat(100)),
        ];
        let all = history_messages(&history, Limit::Chars(606));
        assert_eq!(all.len(), 4);
        assert_eq!(all[1].content.len(), 500);
        let less = history_messages(&history, Limit::Chars(605));
        assert_eq!(less[1], Message::assistant(OMITTED));
        assert_eq!(less[3].content.len(), 100);
    }

    /// Whatever the sizes and the limit: within it, alternating, in order,
    /// starting with a question.
    #[test]
    fn history_stays_within_its_limit_and_well_formed() {
        for size in [0, 1, 20, 300, 5_000] {
            let history: Vec<ChatTurn> = (0..15)
                .flat_map(|n| {
                    [
                        turn(ChatRole::User, &format!("{n} é {}", "q".repeat(size / 7))),
                        turn(ChatRole::Assistant, &"ã".repeat(size + n)),
                    ]
                })
                // A window that starts in the middle of an exchange.
                .skip(1)
                .collect();
            for amount in [0, 1, 5, 12, 13, 25, 40, 150, 1_200, 12_000, 1_000_000] {
                for limit in [Limit::Chars(amount), Limit::Tokens(amount)] {
                    let messages = history_messages(&history, limit);
                    let used: usize = messages.iter().map(|m| limit.cost(&m.content)).sum();
                    assert!(used <= amount, "{size} {limit:?}: {used}");
                    assert!(messages.len() <= MAX_HISTORY_TURNS);
                    for (i, message) in messages.iter().enumerate() {
                        let expected = if i % 2 == 0 { Message::user("") } else { Message::assistant("") };
                        assert_eq!(message.role, expected.role, "{size} {limit:?} at {i}");
                    }
                    let numbers: Vec<u32> =
                        messages.iter().step_by(2).filter_map(|m| m.content.split(' ').next()?.parse().ok()).collect();
                    assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
                }
            }
        }
    }

    #[test]
    fn follow_ups_reuse_the_last_question_for_retrieval() {
        let history = vec![turn(ChatRole::User, "how are tokens rotated"), turn(ChatRole::Assistant, "...")];
        assert_eq!(retrieval_query("and the tests?", &history), "how are tokens rotated and the tests?");
        let long = "explain the whole shard install flow in detail please";
        assert_eq!(retrieval_query(long, &history), long);
        assert_eq!(retrieval_query("hi", &[]), "hi");
    }

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

    fn ask(prompt: &str) -> AskRequest {
        AskRequest { prompt: prompt.into(), provider: None, context_chunks: None, skill: None, history: Vec::new() }
    }

    fn token_settings(max: usize) -> AgentSettings {
        let mut s = AgentSettings::default();
        s.context.max_input_tokens = Some(max);
        s
    }

    #[test]
    fn without_max_input_tokens_the_character_limits_apply() {
        let budget = Budget::plan(&AgentSettings::default(), "sys", &ask("q"));
        assert_eq!(budget.context, Limit::Chars(24_000));
        assert_eq!(budget.history, Limit::Chars(MAX_HISTORY_CHARS));
        assert!(!budget.adaptive && budget.max_input_tokens.is_none());
    }

    #[test]
    fn the_token_budget_pays_fixed_parts_first() {
        let system = "s".repeat(300);
        let budget = Budget::plan(&token_settings(1000), &system, &ask(&"q".repeat(90)));
        let fixed = estimate_message(&system) + estimate_message(&"q".repeat(90)) + FRAMING_TOKENS;
        assert_eq!(budget.context, Limit::Tokens(1000 - fixed));
        assert!(budget.adaptive);
        // A budget smaller than the fixed parts leaves no context, without underflow.
        assert_eq!(Budget::plan(&token_settings(10), &system, &ask("q")).context, Limit::Tokens(0));
    }

    #[test]
    fn history_takes_its_share_before_context() {
        let history = vec![turn(ChatRole::User, &"h".repeat(900)), turn(ChatRole::Assistant, &"a".repeat(300))];
        let mut request = ask("q");
        request.history = history;
        let budget = Budget::plan(&token_settings(2000), "sys", &request);
        // Both turns fit the 1200 token history cap (300 + 100 tokens).
        assert!(matches!(budget.context, Limit::Tokens(n) if n < 2000 - 400));
        let mut tight = token_settings(2000);
        tight.context.max_history_tokens = 150;
        let kept = history_messages(&request.history, Limit::Tokens(150));
        assert_eq!(kept.len(), 2, "the exchange is shortened, not split");
        assert!(kept.iter().map(|m| estimate_tokens(&m.content)).sum::<usize>() <= 150);
        assert_eq!(Budget::plan(&tight, "sys", &request).history, Limit::Tokens(150));
    }

    #[test]
    fn a_big_hit_is_skipped_for_smaller_relevant_ones() {
        let hits = vec![hit("a.rs", &"a".repeat(60)), hit("big.rs", &"b".repeat(3000)), hit("c.rs", &"c".repeat(60))];
        let ctx = assemble_limited(&hits, Limit::Tokens(120));
        let paths: Vec<_> = ctx.sources.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "c.rs"]);
        assert!(estimate_tokens(&ctx.text) <= 120);
    }

    #[test]
    fn the_estimated_context_never_exceeds_the_token_limit() {
        let hits = vec![hit("a.rs", &"x".repeat(5000))];
        let ctx = assemble_limited(&hits, Limit::Tokens(50));
        assert_eq!(ctx.sources.len(), 1);
        assert!(estimate_tokens(&ctx.text) <= 55, "{}", estimate_tokens(&ctx.text));
    }

    #[test]
    fn user_message_mentions_missing_context() {
        assert!(user_message("why?", "").starts_with("No indexed workspace context"));
        assert!(user_message("why?", "ctx\n\n").contains("## Workspace context"));
    }

    /// Benchmark scenarios: the same ranked candidates, assembled with the legacy
    /// character limits and with the token budget. Run with
    /// `cargo test benchmark -- --nocapture` to see the table (docs/benchmarks.md).
    mod benchmark {
        use std::collections::BTreeSet;

        use brok_sdk::prelude::SearchHit;

        use crate::ask::assemble_limited;
        use crate::context::{Limit, deduplicate, estimate_tokens, prefer_modified};

        fn chunk(path: &str, start: u32, end: u32, score: f32, lines: usize) -> SearchHit {
            let content = (start..start + lines as u32)
                .map(|n| format!("    let value_{n} = compute(input_{n}, &config);"))
                .collect::<Vec<_>>()
                .join("\n");
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
                score,
                content,
            }
        }

        struct Outcome {
            relevant: usize,
            sources: usize,
            tokens: usize,
        }

        fn measure(hits: &[SearchHit], limit: Limit, relevant: &BTreeSet<&str>) -> Outcome {
            let context = assemble_limited(hits, limit);
            Outcome {
                relevant: context
                    .sources
                    .iter()
                    .filter(|s| relevant.contains(s.path.as_str()))
                    .map(|s| s.path.as_str())
                    .collect::<BTreeSet<_>>()
                    .len(),
                sources: context.sources.len(),
                tokens: estimate_tokens(&context.text),
            }
        }

        /// Ranked candidates as a search returns them: the top hits are overlapping slices of one
        /// big file, and what the question needs is further down the ranking.
        fn candidates() -> Vec<SearchHit> {
            let mut hits = vec![
                chunk("src/big.rs", 1, 60, 1.00, 60),
                chunk("src/big.rs", 30, 90, 0.98, 60),
                chunk("src/big.rs", 50, 110, 0.96, 60),
                chunk("src/big.rs", 1, 60, 0.95, 60),
            ];
            for (i, path) in ["src/auth.rs", "src/session.rs", "tests/auth.rs", "src/token.rs"].iter().enumerate() {
                hits.push(chunk(path, 10, 30, 0.90 - i as f32 * 0.02, 20));
            }
            for n in 0..8 {
                hits.push(chunk(&format!("src/noise{n}.rs"), 1, 40, 0.5 - n as f32 * 0.01, 40));
            }
            hits
        }

        #[test]
        fn token_budget_keeps_the_relevant_context_in_fewer_tokens() {
            let relevant: BTreeSet<&str> = ["src/auth.rs", "src/session.rs", "tests/auth.rs", "src/token.rs"].into();
            let all = candidates();

            // Legacy: the first 8 chunks, up to 24,000 characters.
            let legacy_hits: Vec<_> = all.iter().take(8).cloned().collect();
            let legacy = measure(&legacy_hits, Limit::Chars(24_000), &relevant);

            // Token-aware: more candidates, overlaps dropped, a 3,000 token context.
            let adaptive = measure(&deduplicate(all.clone()), Limit::Tokens(3_000), &relevant);

            // The same small budget without and with work in progress in src/auth.rs.
            let tight = measure(&deduplicate(all.clone()), Limit::Tokens(1_500), &relevant);
            let dirty: BTreeSet<String> = ["src/auth.rs".to_owned()].into();
            let favored = measure(&deduplicate(prefer_modified(all, &dirty)), Limit::Tokens(1_500), &relevant);

            println!("{:<34} {:>9} {:>8} {:>8}", "scenario", "relevant", "sources", "tokens");
            for (name, o) in [
                ("legacy 8 chunks / 24k chars", &legacy),
                ("tokens 3k + dedup", &adaptive),
                ("tokens 1.5k + dedup", &tight),
                ("tokens 1.5k + dedup + modified", &favored),
            ] {
                println!("{name:<34} {:>6}/4 {:>8} {:>8}", o.relevant, o.sources, o.tokens);
            }

            assert!(adaptive.tokens <= 3_000, "the budget is respected");
            assert!(adaptive.tokens * 10 < legacy.tokens * 7, "at least 30% fewer tokens than the character limits");
            assert!(adaptive.relevant >= legacy.relevant, "nothing relevant is lost");
            assert!(favored.tokens <= 1_500 && favored.relevant >= tight.relevant);
            let first = |hits: Vec<SearchHit>| deduplicate(hits).remove(0).path;
            assert_eq!(first(candidates()), "src/big.rs");
            assert_eq!(
                first(prefer_modified(candidates(), &dirty)),
                "src/auth.rs",
                "the file being changed leads the context"
            );
        }
    }

    /// What truncating the history loses (brokspace/brok-agent#24). A synthetic
    /// conversation states a decision in some questions; each row counts the
    /// decisions still sent to the model. `cargo test history_benchmark --
    /// --nocapture` prints the table (docs/benchmarks.md).
    mod history_benchmark {
        use brok_sdk::prelude::{ChatRole, ChatTurn};

        use super::turn;
        use crate::ask::{MAX_HISTORY_CHARS, estimate_message, history_messages};
        use crate::context::Limit;

        /// Exchanges (of `EXCHANGES`) in which the user states a decision.
        const DECISIONS: [usize; 5] = [1, 5, 10, 15, 19];
        const EXCHANGES: usize = 20;
        /// Turns `brok chat` keeps and sends.
        const CLIENT_TURNS: usize = 12;

        fn conversation(answer_chars: usize) -> Vec<ChatTurn> {
            (1..=EXCHANGES)
                .flat_map(|n| {
                    let question = if DECISIONS.contains(&n) {
                        format!("Decision {n}: we keep the retry limit at {n}. Apply that to the client.")
                    } else {
                        format!("Question {n}: where is this handled and how would you change it?")
                    };
                    [turn(ChatRole::User, &question), turn(ChatRole::Assistant, &"a".repeat(answer_chars))]
                })
                .collect()
        }

        /// Messages kept, decisions among them and their estimated tokens.
        fn measure(history: &[ChatTurn], limit: Limit) -> (usize, usize, usize) {
            let sent = &history[history.len().saturating_sub(CLIENT_TURNS)..];
            let kept = history_messages(sent, limit);
            let decisions = DECISIONS
                .iter()
                .filter(|n| kept.iter().any(|m| m.content.starts_with(&format!("Decision {n}:"))))
                .count();
            (kept.len(), decisions, kept.iter().map(|m| estimate_message(&m.content)).sum())
        }

        #[test]
        fn history_truncation_keeps_few_early_decisions() {
            let limits = [
                ("character limit (default)", Limit::Chars(MAX_HISTORY_CHARS)),
                ("token budget, history 1200", Limit::Tokens(1_200)),
                ("token budget, history 4000", Limit::Tokens(4_000)),
            ];
            println!("{:<28} {:>7} {:>9} {:>10} {:>8}", "limit", "answer", "messages", "decisions", "tokens");
            let mut results = Vec::new();
            for answer in [600, 2_400, 6_000] {
                for (name, limit) in limits {
                    let (messages, decisions, tokens) = measure(&conversation(answer), limit);
                    println!("{name:<28} {answer:>7} {messages:>9} {decisions:>8}/5 {tokens:>8}");
                    results.push((answer, limit, messages, decisions));
                }
            }
            // Every limit keeps what the client's turn cap lets through (the
            // decisions of exchanges 15 and 19), however long the answers are.
            assert!(results.iter().all(|r| r.2 == CLIENT_TURNS && r.3 == 2), "{results:?}");
        }
    }
}
