# AGENTS.md

Rust toolchain is pinned to 1.95.0 (`rust-toolchain.toml`).

After modifying Rust code, always run `cargo clippy` and `cargo test` as a final check.

This project is in active development. Breaking changes to APIs, serialization formats, and persisted data are acceptable — no backward-compatibility shims needed.

## Runtime Config

Set `RUST_LOG` to control tracing output (logs go to stderr). Runtime tooling (shell/glob/grep tools) depends on `fd`, `rg` (ripgrep), and `sh`.

## Frontend UI

When adding shadcn/ui primitives to `app/coda_web`, generate them with the shadcn CLI first using `npx` (for example, `npx shadcn@latest add radio-group`) — not `pnpm dlx` — then adapt the generated component to the local UI.

## Architecture

Cargo workspace implementing an AI Agent:

```
app/
  coda_server (server binary)
  coda_web    (React/TypeScript web dashboard — the primary UI)
crates/
  ├── coda_agent   — agent runtime
  ├── coda_tools   — built-in tool implementations & tool spec system
  ├── coda_core    — shared protocol & abstractions
  ├── coda_openai  — LLM provider implementation
  ├── coda_skills  — skill loading & parsing
  └── coda_mcp     — MCP protocol integration
```

### Crate Responsibilities

- **`coda_core`** — Core abstractions for LLM interaction: `LLMProvider` trait (streaming completions), `Message` type hierarchy (System/User/Assistant/Tool), `Tool`/`ToolObject` traits (tool definition & execution), and `Tools` (tool registry). All other crates depend on this one.
- **`coda_openai`** — OpenAI-compatible `LLMProvider` implementation. Converts `coda_core` message types to `async_openai` SDK types, handles streaming SSE responses, and reassembles tool-call chunks.
- **`coda_agent`** — Agent runtime. Key components: `Agent` (per-agent state & tool set), `AgentSpec` (plain per-agent data; sub-agents referenced by name), `AgentTeam` (a validated, rooted set of specs — its `new` is the single validation gate, `build(workspace_dir)` mints fresh agents per session), `Session` (high-level session facade wrapping agent lifecycle & event dispatch), `AgentRuntime` (low-level multi-agent scheduler), `RunConfig` (session-level configuration bundling the default model, per-agent model overrides, and tool-approval policy), `ModelProfile` (a model paired with sampling parameters — used by `RunConfig` to assign different models to different agents). Supports tool approval (auto/manual/conditional) and sub-agents (stateful/stateless modes).
- **`coda_tools`** — Built-in tool implementations and the tool spec system. Provides 9 built-in tools (`shell`, `read_file`, `write_file`, `edit_file`, `ls`, `grep`, `glob`, `read_todos`, `write_todos`), `TodoItem`, the `ToolSpec` factory trait (with `name()` metadata), `BuildContext`, `PrebuiltToolSpec`, and name-based resolution (`builtin_specs()`, `spec_by_name`). Depends on `coda_core`.
- **`coda_skills`** — Loads skill definitions from `.coda/skills/<name>/SKILL.md` directories. Parses YAML frontmatter (name, description, etc.) and generates XML for system-prompt injection.
- **`coda_mcp`** — MCP (Model Context Protocol) client integration. Supports stdio and HTTP (streamable-http) transports, adapts MCP server tools into `ToolObject` instances via `McpToolAdapter`, auto-prefixes tool names with `mcp__` and truncates to 64 chars. Configuration is read from the `mcpServers` field in a JSON file.
- **`coda_server`** — Application layer: WebSocket server (axum) holding one live `Session` per connection (single-client via latest-wins eviction, single-workspace), ask_user tool, a `Transport` trait (typed `ClientMessage`/`ServerMessage` in/out, hiding framing & serialization) with a WebSocket implementation, system-prompt construction, file-based agent configuration (loads `.coda/agents/` into a validated `AgentTeam` at startup — see below), tool approval config, MCP server loading, and session persistence (JSON file storage). Located at `app/coda_server`. The user-facing client is the `coda_web` dashboard (`app/coda_web`).

### Server Configuration

The server reads `coda-server.toml` (overridable via `CODA_SERVER_CONFIG` env var). It declares providers and workspaces:

- **Providers** — `[[providers]]` array-of-tables. Each is an OpenAI-compatible endpoint with `id`, `kind` (`"generic"` or `"deepseek"`), `api_key` / `base_url` (both support `${VAR}` env expansion), and an inline `models` array. Each model has a required `id` (the API model name sent in requests), an optional `name` (human-readable dashboard label; defaults to `id`), and optional `reasoning_efforts` (omit for non-reasoning models). Models under the same provider share one `Arc<OpenAI>` instance. The dashboard shows a grouped dropdown (provider → model) and a reasoning-effort selector when the selected model has reasoning levels.
- **Workspaces** — `[[workspaces]]` array-of-tables with `id` and `path`. Sessions are scoped to a workspace and persisted under `.coda/sessions/`.

Selection keys on the wire are composite (`{provider_id}:{model_id}`). The first model of the first provider is the default.

### Key Abstractions

- **`LLMProvider`** (`coda_core::llm`) — Model provider trait; core method `stream()` returns `Stream<LLMStreamEvent>`.
- **`Tool` / `ToolObject`** (`coda_core::tool`) — `Tool` is a generic trait (associated types Parameters/Output); `ToolObject` is the object-safe, dynamically-dispatched counterpart. `ToolWrapper` bridges the two.
- **`ToolSpec` / `BuildContext`** (`coda_tools::spec`) — `ToolSpec` is a factory trait for creating tool instances; `BuildContext` carries workspace directory and optional todo store during tool construction.
- **`AgentSpec` / `AgentTeam`** (`coda_agent::spec`) — `AgentSpec` is plain per-agent data (sub-agents referenced by name). `AgentTeam::new(root, subagents)` validates the whole set once (unique names, resolvable references, tool/sub-agent namespace conflicts; sub-agents unreachable from the root are dropped) so holding one proves it sound; `AgentTeam::build(workspace_dir)` then constructs fresh `Agent`s per session (infallibly).
- **`Session`** (`coda_agent::session`) — High-level API for callers: send tasks, consume events, resume from suspension, and shut down sessions. `SessionBuilder::team(&AgentTeam, workspace_dir)` borrows the team and builds the agents at `open()`.

### Agent Configuration (file-based)

Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`: YAML frontmatter (`description`, `mode` = stateful/stateless, `tools`, `subagents`, `model`, `reasoning_effort`) plus a markdown body used as the system prompt. They become sub-agents of the top-level `coda` agent and may reference one another by name to form deeper graphs (sharing allowed).

Agents may optionally override the session’s model via the `model` frontmatter field, a `{provider_id}:{model_id}` selection key (optionally paired with `reasoning_effort`). Overrides are validated against the provider catalog at startup — an unknown model or unsupported reasoning effort is a hard error. When a sub-agent omits `model`, it inherits the session’s default (root) model.

The `coda` agent itself is configured by an optional `.coda/agents/AGENT.md` (a bare file, not a directory): its `tools`, `subagents`, and body each *explicitly override* a default when present (otherwise: all tools, the auto-attached unreferenced agents, and the built-in `system-prompt.md` base prompt). `coda` is always present.

Tools resolve by name against built-ins plus prebuilt tools (e.g. MCP tools from `mcp.json`). A name ending in `*` is a prefix pattern — `mcp__example__*` enables every tool that server exposes; a bare `*` is not a wildcard. To grant every tool, omit `tools` on the root `coda` agent (whose default is all tools) — a sub-agent that omits `tools` gets none. Unknown plain tool names, duplicate agent names, dangling sub-agent references, and tool/sub-agent namespace conflicts are hard startup errors; a pattern that matches nothing only warns. Sub-agents unreachable from `coda` are ignored with a warning.
