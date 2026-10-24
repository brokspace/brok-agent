# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-24

## [0.1.0] - Unreleased

### Added

- Incremental change tracking through `git diff --name-only HEAD` and `git ls-files --others`, across the
  root repository and every initialized submodule; publishes `file:changed`.
- Optional pre-indexation LLM review of changed files selected by glob filters; publishes `review:result`.
- `agent:ask` handler: hybrid search through the core index service, Markdown context assembly within a
  character budget, secret redaction, and streamed answers (`agent:ask.chunk` → `agent:ask.done`).
- OpenAI-compatible chat completions client with a robust incremental SSE parser (Ollama, LM Studio,
  Anthropic, DeepSeek, OpenAI, vLLM, ...).
- Prompt validation and redaction of AWS, GitHub, Slack, `sk-` style keys, PEM private keys and
  `password = "..."` style assignments.
