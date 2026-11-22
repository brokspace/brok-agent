<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/brokspace-on-dark.svg">
    <img src=".github/assets/brokspace.svg" alt="Brokspace" height="44">
  </picture>
</p>
# brok-agent

The default system shard of [Brokspace](https://github.com/brokspace/brok). `brok init` installs it and
`brok run` starts it next to the event bus.

| Responsibility | How |
|---|---|
| **Change tracking** | Every few seconds, in every repository of the workspace (the root repository, its submodules and independent repositories cloned inside it), fingerprints dirty files (`git diff --name-only HEAD` + untracked files) and follows `HEAD`: after a commit, pull, checkout or rebase, the files that differ between the old and new commit are reported too. Only what changed is published as `file:changed`, so the core indexer stays fresh. |
| **Pre-indexation review** | Optionally sends changed files matching glob filters (e.g. `src/api/**`, never `**/*.md`) to an LLM before publishing `file:changed`, and reports a `review:result` with a `pass` / `warn` / `fail` verdict. |
| **Answers (RAG)** | Handles `brok ask`: hybrid search over the local index, context assembly within a budget, secret redaction, then streams the answer from any OpenAI-compatible API. |

## Multi-repo workspaces

A workspace does not have to be a single repository. Point Brokspace at a
folder that holds several independent clones (`~/work/acme/{api,web,infra}`)
and the agent tracks each of them: repositories are discovered up to four
levels deep (hidden folders, `node_modules`, `target`, `dist`, `build` and
`vendor` are skipped) and rediscovered every minute, so a fresh `git clone`
is picked up without a restart.

## Install

```sh
cargo install brok-agent   # put it on PATH (or next to the `brok` binary)
brok init                  # finds it and installs it into .brokspace/shards/
```

`brok init` discovers the agent through `brok-agent --manifest`, which prints
the `shard.json` compiled into the binary. Alternatively:
`brok shard install https://github.com/brokspace/brok-agent`.

## Configuration

Settings live in `.brokspace/brok.yml` under `shards.brok-agent.settings`; LLM providers come from the
global `llm` section. Every key is optional.

```yaml
llm:
  default: ollama
  providers:
    ollama: { base_url: http://localhost:11434/v1, model: qwen2.5-coder:7b }
    anthropic: { base_url: https://api.anthropic.com/v1, model: claude-sonnet-5-5, api_key_env: ANTHROPIC_API_KEY }

shards:
  brok-agent:
    enabled: true
    settings:
      watch: auto
      context_chunks: 8
      max_context_chars: 24000
      review:
        enabled: true
        include: ["src/api/**"]
        exclude: ["**/*.md"]
        provider: ollama
```

| Setting | Default | Meaning |
|---|---|---|
| `watch` | `auto` | How changes are detected. `auto`: file system events, plus a reconciliation scan every `reconcile_interval_secs`. `native`: events only (a scan at start and after lost events). `polling`: a scan every `watch_interval_secs`, as before. If the workspace cannot be watched (for example the system's watch limit), the agent logs one warning and polls. |
| `watch_interval_secs` | `5` | Seconds between two git scans when polling. |
| `debounce_ms` | `300` | Quiet time after the last event before a burst of changes is scanned (a build or a checkout touches thousands of files). |
| `reconcile_interval_secs` | `300` | In `auto` mode, how often a full scan recovers from events the OS did not deliver. `0` turns it off. |
| `context_chunks` | `8` | Chunks retrieved per question (overridable per request). |
| `max_context_chars` | `24000` | Context budget sent to the model. |
| `system_prompt` | built in | Replaces the default system prompt. |
| `review.enabled` | `false` | Turns the pre-indexation review on. |
| `review.include` / `review.exclude` | `["src/api/**"]` / `["**/*.md"]` | Globs relative to the workspace root (`*` does not cross `/`). |
| `review.provider` | `llm.default` | Provider used for reviews. |
| `review.max_file_kb` | `32` | Bytes of each file sent for review. |

With events, an idle workspace costs no `git` process at all (polling starts about three per repository every
`watch_interval_secs`). Directories that are ignored by git, hidden build output (`node_modules`, `target`, `dist`,
`build`, `vendor`) and `.brokspace/` are not watched; commits, pulls, checkouts and rebases are noticed through each
repository's `HEAD` and refs, including those of submodules and linked worktrees. git stays the source of truth: an
event only makes the agent run its usual scan.

API keys are **never** stored in `brok.yml`: a provider names the environment variable that holds its key
(`api_key_env`). The shard manifest only lets `*_API_KEY`, `OPENAI_*` and proxy variables through.

## Events

| Direction | Topic | Payload |
|---|---|---|
| publishes | `file:changed` | `FileChanged { changed, deleted }` |
| publishes | `review:result` | `ReviewResult { path, verdict, summary }` |
| subscribes | `agent:ask` | `AskRequest { prompt, provider?, context_chunks? }` |
| replies | `agent:ask.chunk` | `AskChunk { text }`, streamed |
| replies | `agent:ask.done` / `agent:ask.error` | `AskDone { provider, model, sources }` / `AskFailed { message }` |
| requests | `index:search` | `SearchRequest` → `SearchResults`, answered by the core |

Payload types are defined in [`brok-sdk`](https://github.com/brokspace/brok-sdk).

## Guardrails

- Prompts must be non-empty and at most 16 000 characters.
- Retrieved context and files sent for review are scrubbed of AWS keys, GitHub/Slack tokens, `sk-` style
  API keys, PEM private keys and `password = "..."` style assignments before leaving the machine.

## Development

Clone this repository next to `brok-sdk` (it is a path dependency):

```text
brokspace/
├── brok-sdk/
└── brok-agent/
```

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

To iterate on the agent against a real workspace, install your working copy into the workspace and start
the runtime:

```sh
brok shard install ../brok-agent   # builds with the manifest's build command
brok run                            # starts the bus and the agent
brok ask "where are bus replies routed?"
```

You can also run it in your terminal while the bus is up: disable it in `brok.yml`
(`shards.brok-agent.enabled: false`), then start it with the endpoint and a token issued by the runtime
(`BROK_ENDPOINT`, `BROK_TOKEN`), e.g. `RUST_LOG=brok_agent=debug cargo run`. Logs go to stderr.

## Releasing

Bump the version in a PR to `staging`, merge the release PR into `main`, then push a `vX.Y.Z` tag on
`main` (`git tag vX.Y.Z origin/main && git push origin vX.Y.Z`). The release workflow builds binaries for Linux, macOS and Windows (x86_64 and
aarch64), attaches them with `.sha256` files and prints the `releases` block to paste into
`shard.json`, so `brok shard install` downloads a checksum-verified binary instead of compiling.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your
option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
