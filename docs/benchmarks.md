# Context benchmarks

## Retrieved context

`cargo test benchmark -- --nocapture` assembles the same ranked candidates three ways and prints how much of the relevant context each keeps and what it costs. The scenario is synthetic and deterministic (no model, no index): the top hits are overlapping slices of one large file, the four files the question needs rank below them, and eight unrelated files follow.

| scenario | relevant files | sources | estimated tokens |
|---|---|---|---|
| legacy: first 8 chunks, 24,000 characters | 4 of 4 | 8 | about 5,100 |
| token budget 3,000 + deduplication | 4 of 4 | 6 | about 2,900 |
| token budget 1,500 + deduplication | 1 of 4 | 2 | about 1,300 |
| token budget 1,500 + deduplication + modified files first | 1 of 4 | 2 | about 1,300 |

What it shows:

- Deduplication and the token budget keep everything relevant for about 45% fewer tokens than the character limits.
- A budget that is too small for the question loses context: 1,500 tokens does not fit this question whatever the ordering. The budget is the user's choice (`context.max_input_tokens`); the agent logs the estimate so it can be tuned.
- Preferring modified files changes the order, not the budget: the file you are working on (`src/auth.rs`) leads the context instead of the large file that merely ranked first.

The tokens are the agent's conservative estimate (it over-counts), and only counts are logged, never prompt or source text. The test asserts the budget is respected, that nothing relevant is lost against the legacy limits and that the modified file leads, so a regression in the assembly fails CI.

## Conversation history

`cargo test history_benchmark -- --nocapture` truncates a synthetic conversation of 20 exchanges the way a request does today: `brok chat` sends the last 12 messages and the agent keeps the newest that fit. The user states a decision in 5 of the questions; the table counts the ones the model still receives. Answers have a fixed length in characters.

| limit | answer length | messages before | decisions before | messages now | decisions now | tokens now |
|---|---:|---:|---:|---:|---:|---:|
| 12,000 characters (default) | 600 | 12 | 2 of 5 | 12 | 2 of 5 | about 1,400 |
| 12,000 characters (default) | 2,400 | 8 | 1 of 5 | 12 | 2 of 5 | about 3,400 |
| 12,000 characters (default) | 6,000 | 2 | 0 of 5 | 12 | 2 of 5 | about 2,200 |
| token budget, history 1,200 (its default) | 600 | 10 | 1 of 5 | 12 | 2 of 5 | about 1,200 |
| token budget, history 1,200 (its default) | 2,400 | 2 | 0 of 5 | 12 | 2 of 5 | about 1,000 |
| token budget, history 1,200 (its default) | 6,000 | 0 | 0 of 5 | 12 | 2 of 5 | about 1,200 |
| token budget, history 4,000 | 600 | 12 | 2 of 5 | 12 | 2 of 5 | about 1,400 |
| token budget, history 4,000 | 2,400 | 8 | 1 of 5 | 12 | 2 of 5 | about 3,400 |
| token budget, history 4,000 | 6,000 | 2 | 0 of 5 | 12 | 2 of 5 | about 2,200 |

Token figures include a fixed overhead per message, which the history limit does not count.

What it shows:

- The agent now keeps the newest question, then earlier questions, then the newest answer (shortened if needed), then earlier answers that fit; an answer left out is replaced by a short note. Before, it stopped at the first message that did not fit, so one long answer could remove the whole history.
- The 12 message cap is now the only thing that drops decisions here: anything older than six exchanges is still lost. Keeping those would need a higher cap or summaries.

It measures what is sent, not answer quality: no model is involved, and real conversations differ.

