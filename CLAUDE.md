# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build
cargo run -p coda_cli
cargo test
cargo test -p coda_skills   # run tests for a specific crate
cargo clippy
cargo fmt
```

## Runtime Config

A `.env` file is required with:

```
OPENAI_API_KEY=...
OPENAI_BASE_URL=...
OPENAI_MODEL=...
```

Set `RUST_LOG` to control tracing output (logs go to stderr). Runtime tooling depends on `fd`, `rg` (ripgrep), and `sh`.

## Architecture

A Cargo workspace implementing an AI agent CLI:

```
coda_cli (app binary)
  ├── coda_agent
  ├── coda_core
  ├── coda_openai
  └── coda_skills
```

- **`coda_core`** — shared protocol and abstractions for model interaction and tools.
- **`coda_openai`** — OpenAI-compatible model provider implementation.
- **`coda_agent`** — agent runtime: conversation state, tool orchestration, and approval-aware execution flow.
- **`coda_skills`** — loads and parses skills.
- **`coda_cli`** — interactive terminal app that wires provider + agent + skills, currently loads skills from `./.coda/skills/`, streams responses, and handles user approval for sensitive tool calls.
