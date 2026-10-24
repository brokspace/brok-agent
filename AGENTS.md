# AGENTS.md

Operating guide for coding agents and contributors working in `brokspace/brok-agent`: the default **system shard** of Brokspace. It tracks workspace changes through git, optionally reviews changed files with an LLM before they are indexed, and answers `brok ask` / `brok skill run` with retrieval augmented generation over any OpenAI-compatible API (Ollama, LM Studio, hosted providers).

Keep the agent small and general. Anything domain specific (a security auditor, a ticket bot, a team review policy) belongs in its own shard. See [brok's SCOPE.md](https://github.com/brokspace/brok/blob/main/SCOPE.md).

## Layout

```text
src/
  main.rs        connect to the bus, dispatch events, `--manifest` prints the embedded shard.json
  settings.rs    shard settings (shards.brok-agent.settings in brok.yml) and LLM routing
  tracker.rs     git based change tracking (root repo and submodules) -> file:changed
  review.rs      pre-indexation review -> review:result
  ask.rs         RAG answers: search via the index service, context assembly, streaming replies
  llm.rs         OpenAI-compatible chat completions client with an incremental SSE parser
  guardrails.rs  prompt validation and secret redaction (applied before context leaves the machine)
shard.json       the manifest; compiled into the binary and parsed in a unit test
```

## Build and test

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The agent depends on `brok-sdk` from crates.io. For polyrepo development, check out [brok-sdk](https://github.com/brokspace/brok-sdk) next to this repository and run `brok/scripts/bootstrap.sh` (it writes `../.cargo/config.toml` patching `brok-sdk` to the sibling checkout). To try the agent for real, install it into a workspace with `brok shard install ../brok-agent` and run `brok run -v`.

## Conventions

- Log to **stderr** with `tracing`; `brok run` captures the output line by line. Never print secrets or prompts at `info` level.
- Everything that leaves the machine (context sent to an LLM) goes through `guardrails::redact_secrets`.
- API keys only ever come from the environment variable named by `api_key_env`; the manifest's `permissions.env` must allow it.
- Keep `shard.json` permissions minimal. Widening them (new topics, env vars, network) is a one-way door for users who already granted consent: call it out in the PR.
- Pure functions for logic worth testing (diffing snapshots, SSE parsing, context assembly, redaction); tests use real git repositories in temp dirs where git is involved.
- Commit messages and PR titles follow Conventional Commits: `feat(ask): ...`, `fix(tracker): ...`.
- No em-dashes in prose.

## Agent skills configuration

- **Issue tracker**: GitHub Issues on [brokspace/brok-agent](https://github.com/brokspace/brok-agent/issues) via `gh`. See [docs/agents/issue-tracker.md](docs/agents/issue-tracker.md).
- **Triage labels**: [docs/agents/triage-labels.md](docs/agents/triage-labels.md). Issues are judged against [brok's SCOPE.md](https://github.com/brokspace/brok/blob/main/SCOPE.md).
- **Domain docs**: the shared glossary is [brok's GLOSSARY.md](https://github.com/brokspace/brok/blob/main/GLOSSARY.md); decisions live in [brok/docs/adr](https://github.com/brokspace/brok/tree/main/docs/adr).
