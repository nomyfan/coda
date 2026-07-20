# AGENTS.md

Rust toolchain is pinned to 1.95.0 (`rust-toolchain.toml`).

After modifying Rust code, always run `cargo clippy` and `cargo test` as a final check.

This project is in active development. Breaking changes to APIs, serialization formats, and persisted data are acceptable — no backward-compatibility shims needed.

## Git Workflow

- Use Conventional Commits format for commit messages and pull request titles.
- Every commit must include a `Co-authored-by` trailer that names the **AI agent** which made the change — not the human. The human is already the commit author, so listing them as co-author is redundant and wrong. Use the agent's own identity, optionally followed by the model's display name (e.g. `Opus 4.8` — not a slug like `claude-opus-4-8`):

  ```
  Co-authored-by: Claude Opus 4.8 <noreply@anthropic.com>
  Co-authored-by: Codex GPT-5 <codex@openai.com>
  ```

## Runtime Config

**Unix-only for now.** The runtime targets Unix (Linux/macOS): the `shell` tool runs every command through `bash -c` (`bash` is the current concrete backend behind the platform-agnostic `shell` tool name), and the env probes use Unix utilities (`uname`/`sw_vers`). Windows is not supported at this stage.

Set `RUST_LOG` to control tracing output (logs go to stderr). Runtime tooling (shell/glob/grep tools) depends on `fd`, `rg` (ripgrep), and `bash`.

## Frontend UI

When adding shadcn/ui primitives to `app/coda_web`, generate them with the shadcn CLI first using `npx` (for example, `npx shadcn@latest add radio-group`) — not `pnpm dlx` — then adapt the generated component to the local UI.

After modifying `app/coda_web` code, always run `pnpm --filter coda-web lint` (oxlint) and `pnpm --filter coda-web test` as final checks.

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
- **`coda_server`** — Application layer: axum WebSocket server speaking JSON-RPC 2.0 over a single connection, with live `Session`s owned by the process-level `SessionHub` independently of connections (latest attachment wins per session, and running turns survive disconnects), ask_user tool, a `Transport` trait that receives raw frame text and sends `RpcOutgoing` envelopes, request/notification dispatch, system-prompt construction, file-based agent configuration (loads `.coda/agents/` into a validated `AgentTeam` at startup — see below), tool approval config, MCP server loading, and session persistence (JSON file storage). Located at `app/coda_server`. The user-facing client is the `coda_web` dashboard (`app/coda_web`).

### Server Configuration

The server reads `coda-server.toml` (overridable via `CODA_SERVER_CONFIG` env var). It declares providers and workspaces:

- **Providers** — `[[providers]]` array-of-tables. Each is an OpenAI-compatible endpoint with `id`, `kind` (`"generic"` or `"deepseek"`), `api_key` / `base_url` (both support `${VAR}` env expansion), and an inline `models` array. Each model has a required `id` (the API model name sent in requests), an optional `name` (human-readable dashboard label; defaults to `id`), a required positive `context_window` token count, optional `reasoning_efforts` (array of arbitrary strings passed to the provider API; `"off"` is reserved for turning thinking off — omit it if the model doesn't support disabling thinking; omit the entire field for non-reasoning models), and optional `input_modalities` (list of `"text"` and/or `"image"`; defaults to `["text"]` — add `"image"` to enable image attachments for that model). Models under the same provider share one `Arc<OpenAI>` instance. The dashboard shows a grouped dropdown (provider → model) and a reasoning-effort selector when the selected model has reasoning levels.
- **Workspaces** — `[[workspaces]]` array-of-tables with `id` and `path`. Sessions are scoped to a workspace and persisted under `.coda/sessions/`.
- **Relay** — optional `[relay]` table tuning the process-level session relay's (`coda_server::hub::SessionHub`) per-session in-memory event buffering: `max_log_events` (soft cap on buffered events per turn, default 8192) and `max_message_tier_events` (hard cap on buffered message-tier events per turn, default 4096; exceeding it forces a resync from the persisted state rather than buffering without bound). Both fall back to their default independently when absent.

Selection keys on the wire are composite (`{provider_id}:{model_id}`). The first model of the first provider is the default.

### Workspace Approval Configuration

Tool approval rules live in each workspace's `.coda/config.toml`. Regular tools matching `[permissions.tools].approval_required` patterns suspend for human approval; by default this is `["edit_file", "write_file", "ls", "grep", "glob"]`. Use `mcp__server__*` to gate every tool from one MCP server. The `ask_user` tool is always interactive and always suspends to open the web UI.

Shell approvals use `[permissions.shell]` allow/deny glob lists. A `shell` call auto-approves only when every decomposed simple command matches `allow`, no simple command matches `deny`, and the command uses only statically-vetted sequencing/pipe constructs; other shell constructs suspend for approval.

### Key Abstractions

- **`LLMProvider`** (`coda_core::llm`) — Model provider trait; core method `stream()` returns `Stream<LLMStreamEvent>`.
- **`Tool` / `ToolObject`** (`coda_core::tool`) — `Tool` is a generic trait (associated types Parameters/Output); `ToolObject` is the object-safe, dynamically-dispatched counterpart. `ToolWrapper` bridges the two.
- **`ToolSpec` / `BuildContext`** (`coda_tools::spec`) — `ToolSpec` is a factory trait for creating tool instances; `BuildContext` carries workspace directory and optional todo store during tool construction.
- **`AgentSpec` / `AgentTeam`** (`coda_agent::spec`) — `AgentSpec` is plain per-agent data (sub-agents referenced by name). `AgentTeam::new(root, subagents)` validates the whole set once (unique names, resolvable references, tool/sub-agent namespace conflicts; sub-agents unreachable from the root are dropped; each retained sub-agent's `agent__`-prefixed tool name must fit the 64-character provider limit) so holding one proves it sound; `AgentTeam::build(workspace_dir)` then constructs fresh `Agent`s per session (infallibly).
- **`Session`** (`coda_agent::session`) — High-level API for callers: send tasks, consume events, resume from suspension, and shut down sessions. `SessionBuilder::team(&AgentTeam, workspace_dir)` borrows the team and builds the agents at `open()`.

### Agent Configuration (file-based)

Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`: YAML frontmatter (`description`, `mode` = stateful/stateless, `tools`, `subagents`, `env`, `workspace`, `model`, `reasoning_effort`) plus a markdown body used as the system prompt. They become sub-agents of the top-level `coda` agent and may reference one another by name to form deeper graphs (sharing allowed).

The runtime exposes sub-agents to the LLM as `agent__<name>` tools and strips the prefix for routing. The prefixed name is preserved in live events and session history so clients can identify sub-agent invocations directly. Each reachable file-based agent name may contain up to 57 characters, leaving room for the `agent__` prefix within the 64-character provider limit.

Agents may optionally override the session’s model via the `model` frontmatter field, a `{provider_id}:{model_id}` selection key (optionally paired with `reasoning_effort`). Overrides are validated against the provider catalog at startup — an unknown model or unsupported reasoning effort is a hard error. When a sub-agent omits `model`, it inherits the session’s default (root) model.

Each agent's system prompt is assembled per turn from three independently-lived segments: a **base** body (the `AGENT.md` body / built-in prompt; read once at workspace load), the **workspace knowledge** (`AGENTS.md` + skills for that agent's workspace; refreshed in place by a per-workspace watcher), and a per-turn **env** block. The `env:` frontmatter list selects env fields (`date`, `system`, `shell`, `workspace`); omitting it defaults to `[date]`. Only the date is recomputed each turn (so it never goes stale); the OS is probed once and only when requested, and the shell is fixed to `bash` (the interpreter the `shell` tool always runs commands through).

The `workspace:` frontmatter roots an agent at its own directory (its tool root and knowledge source) — absolute, or relative to the root workspace; absent inherits the root (session) workspace. A `workspace:` that doesn't resolve to an existing directory is a hard startup error. Agents sharing a workspace share one knowledge handle + watcher. This is a default cwd/relative-root, **not** a sandbox — tools can still reach outside it. A per-agent workspace does **not** load its own `.coda/agents` (agent topology is defined only in the root workspace). Only the **workspace knowledge** (`AGENTS.md` + skills) hot-reloads, via the watcher; agent bodies and all frontmatter (`tools`/`subagents`/`mode`/`model`/`workspace`/`env`) are read once at load and need a restart to change. MCP tools and the approval policy remain rooted at the session workspace / session-global.

The `coda` agent itself is configured by an optional `.coda/agents/AGENT.md` (a bare file, not a directory): its `tools`, `subagents`, and body each *explicitly override* a default when present (otherwise: all tools, the auto-attached unreferenced agents, and the built-in `system-prompt.md` base prompt). `coda` is always present.

Tools resolve by name against built-ins plus prebuilt tools (e.g. MCP tools from `mcp.json`). A name ending in `*` is a prefix pattern — `mcp__example__*` enables every tool that server exposes; a bare `*` is not a wildcard. To grant every tool, omit `tools` on the root `coda` agent (whose default is all tools) — a sub-agent that omits `tools` gets none. Unknown plain tool names, duplicate agent names, dangling sub-agent references, and tool/sub-agent namespace conflicts are hard startup errors; a pattern that matches nothing only warns. Sub-agents unreachable from `coda` are ignored with a warning.
