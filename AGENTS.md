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

## Git workflow

Every change reaches `main` through `staging`, the default branch:

```text
your branch --PR, squash--> staging --release PR, merge commit--> main --> tag
```

1. Branch from staging: `git fetch origin && git switch -c <type>/<topic> origin/staging` (for example `feat/kotlin-chunker`, `fix/empty-config`).
2. Commit in small steps with Conventional Commits. End every commit message with a `Co-Authored-By:` trailer naming the agent, for example `Co-Authored-By: Claude <noreply@anthropic.com>`: the PR gets the `ai-agent` label from it.
3. Before pushing, run the fmt, clippy and test commands from "Build and test" above.
4. Push, open the PR against staging with the template filled in, and turn on auto-merge:

   ```sh
   git push -u origin HEAD
   gh pr create --base staging --fill
   gh pr merge --auto --squash
   ```

   The PR merges itself once `ubuntu-latest`, `macos-latest`, `windows-latest` and `policy / title` pass and review conversations are resolved. No approval is needed.
5. A check failed? Read it (`gh pr checks --watch`, `gh run view --log-failed`), fix it on the same branch and push; auto-merge stays on. Cannot fix it? Say what you tried in a PR comment and stop. Never weaken or skip a check to get green.

Never push to `staging` or `main` (both reject it), force-push a branch someone else pushed to, open or merge PRs into `main`, or merge with `--admin`. The release PR into `main` is opened by the Promote workflow and merged by a maintainer.

Do not commit a `Cargo.lock` change that only adds `[[patch.unused]]`: it comes from the local `../.cargo/config.toml` patch, and CI rejects it. Restore the file (`git restore Cargo.lock`) before committing.

## Agent skills configuration

- **Issue tracker**: GitHub Issues on [brokspace/brok-agent](https://github.com/brokspace/brok-agent/issues) via `gh`. See [docs/agents/issue-tracker.md](docs/agents/issue-tracker.md).
- **Triage labels**: [docs/agents/triage-labels.md](docs/agents/triage-labels.md). Issues are judged against [brok's SCOPE.md](https://github.com/brokspace/brok/blob/main/SCOPE.md).
- **Domain docs**: the shared glossary is [brok's GLOSSARY.md](https://github.com/brokspace/brok/blob/main/GLOSSARY.md); decisions live in [brok/docs/adr](https://github.com/brokspace/brok/tree/main/docs/adr).
