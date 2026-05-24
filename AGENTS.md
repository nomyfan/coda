# AGENTS.md

Rust toolchain is pinned to 1.95.0 (`rust-toolchain.toml`).

After modifying Rust code, always run `cargo clippy` and `cargo test` as a final check.

This project is in active development. Breaking changes to APIs, serialization formats, and persisted data are acceptable — no backward-compatibility shims needed.

## Runtime Config

Set `RUST_LOG` to control tracing output (logs go to stderr). Runtime tooling (shell/glob/grep tools) depends on `fd`, `rg` (ripgrep), and `sh`.

## Architecture

Cargo workspace implementing an AI Agent CLI:

```
coda_examples (example binaries: cli, client, server)
  ├── coda_agent   — agent runtime
  ├── coda_core    — shared protocol & abstractions
  ├── coda_openai  — LLM provider implementation
  ├── coda_skills  — skill loading & parsing
  └── coda_mcp     — MCP protocol integration
```

### Crate Responsibilities

- **`coda_core`** — Core abstractions for LLM interaction: `LLMProvider` trait (streaming completions), `Message` type hierarchy (System/User/Assistant/Tool), `Tool`/`ToolObject` traits (tool definition & execution), and `ToolSet` (tool registry). All other crates depend on this one.
- **`coda_openai`** — OpenAI-compatible `LLMProvider` implementation. Converts `coda_core` message types to `async_openai` SDK types, handles streaming SSE responses, and reassembles tool-call chunks.
- **`coda_agent`** — Agent runtime. Key components: `Agent` (per-agent state & tool set), `AgentSpec` (declarative agent-tree builder), `Session` (high-level session facade wrapping agent lifecycle & event dispatch), `AgentRuntime` (low-level multi-agent scheduler), and built-in tools (shell, file read/write, glob, grep, todo). Supports tool approval (auto/manual/conditional) and sub-agents (stateful/stateless modes).
- **`coda_skills`** — Loads skill definitions from `.coda/skills/<name>/SKILL.md` directories. Parses YAML frontmatter (name, description, etc.) and generates XML for system-prompt injection.
- **`coda_mcp`** — MCP (Model Context Protocol) client integration. Supports stdio and HTTP (streamable-http) transports, adapts MCP server tools into `ToolObject` instances via `McpToolAdapter`, auto-prefixes tool names with `mcp__` and truncates to 64 chars. Configuration is read from the `mcpServers` field in a JSON file.
- **`coda_examples`** — Example applications: interactive CLI entry point, system-prompt construction, MCP server loading, and session persistence (JSON file storage).

### Key Abstractions

- **`LLMProvider`** (`coda_core::llm`) — Model provider trait; core method `stream()` returns `Stream<LLMStreamEvent>`.
- **`Tool` / `ToolObject`** (`coda_core::tool`) — `Tool` is a generic trait (associated types Parameters/Output); `ToolObject` is the object-safe, dynamically-dispatched counterpart. `ToolWrapper` bridges the two.
- **`AgentSpec` -> `Agent`** (`coda_agent::spec`) — Declarative spec that builds an agent tree; `build()` recursively creates all agents and detects name collisions.
- **`Session`** (`coda_agent::session`) — High-level API for callers: send tasks, consume events, resume from suspension, and shut down sessions.
