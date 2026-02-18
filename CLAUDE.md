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

Set `RUST_LOG` to control tracing output (logs go to stderr). Agent tools also require `fd`, `rg` (ripgrep), and `sh` at runtime.

## Architecture

A Cargo workspace implementing an AI agent CLI.

```
coda_cli (app binary)
  ├── coda_agent
  │     ├── coda_core
  │     └── coda_openai ── coda_core
  └── coda_skills  (no coda_core dependency)
```

- **`coda_core`** — `LLMProvider` trait (event-stream based via `LLMStreamEvent`), `Tool`/`ToolManager` abstractions, and shared message types
- **`coda_openai`** — `LLMProvider` implementation for OpenAI-compatible APIs (streaming)
- **`coda_agent`** — `Agent<P: LLMProvider>` with stateful conversation history, todos, built-in tools (`read_file`, `write_file`, `ls`, `grep`, `glob`, `shell`, `read_todos`, `write_todos`), and an event-driven `run` loop (`AgentEvent`/`RunConfig`) that orchestrates LLM + tool execution
- **`coda_skills`** — discovers skills from `./skills/` subdirectories, each containing a `SKILL.md` with YAML front matter; serializes them to XML for injection into the system prompt
- **`coda_cli`** — wires everything together; runs an interactive REPL that consumes `Agent::run` events for streaming output and tool activity
