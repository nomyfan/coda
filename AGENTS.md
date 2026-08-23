# AGENTS.md

Rust toolchain is pinned to 1.95.0 (`rust-toolchain.toml`).

After modifying Rust code, always run `cargo clippy` and `cargo test` as a final check.

This project is in active development. Breaking changes to APIs, serialization formats, and persisted data are acceptable — no backward-compatibility shims needed.

## Rust Test Organization

Keep `#[cfg(test)] mod tests { ... }` inline only while it's small. Once it grows past a couple hundred lines, move it to a sibling `<file>_tests.rs` and declare it with `#[path]` instead of a bare `mod tests;` (so line-count tooling can tell test code from real code by filename):

```rust
#[cfg(test)]
#[path = "hub_tests.rs"]
mod tests;
```

If that sibling file itself grows large enough to cover several distinct categories of cases (see `app/coda_server/src/hub_tests/` and `crates/coda_agent/src/runtime/driver_tests/` for the pattern), split it further into a `<file>_tests/` directory: `mod.rs` just declares the submodules, `fixtures.rs` holds shared test doubles/helpers (visibility `pub(super)`, so sibling category modules reach them via `use super::fixtures::*;`), and each remaining file covers one coherent theme (e.g. `approval.rs`, `rewind.rs`). Point the source file's `#[path]` at `<file>_tests/mod.rs`. Each category file still needs `use super::super::*;` to reach the source module's own (private) items — that's `super::super`, not `super`, once tests live two directories down.

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
  ├── coda_ptc     — bounded QuickJS programmatic tool runtime & host bridge
  ├── coda_core    — shared protocol & abstractions
  ├── coda_openai  — LLM provider implementation
  ├── coda_skills  — skill loading & parsing
  └── coda_mcp     — MCP protocol integration
```

### Crate Responsibilities

- **`coda_core`** — Core abstractions for LLM interaction: `LLMProvider` trait (streaming completions), `Message` type hierarchy (System/User/Assistant/Tool), `Tool`/`ToolObject` traits (tool definition & execution), `ThreadState` (durable per-thread tool state), and `Tools` (tool registry). All other crates depend on this one.
- **`coda_openai`** — OpenAI-compatible `LLMProvider` implementation. Converts `coda_core` message types to `async_openai` SDK types, handles streaming SSE responses, and reassembles tool-call chunks.
- **`coda_agent`** — Agent runtime. Key components: `Agent` (per-agent state & tool set), `AgentSpec` (plain per-agent data; sub-agents referenced by name), `AgentTeam` (a validated, rooted set of specs — its `new` is the single validation gate, `build(workspace_dir)` mints fresh agents per session), `Session` (high-level session facade wrapping agent lifecycle & event dispatch), `AgentRuntime` (low-level multi-agent scheduler), `RunConfig` (session-level configuration bundling the default model, per-agent model overrides, and tool-approval policy), `ModelProfile` (a model paired with sampling parameters — used by `RunConfig` to assign different models to different agents). Supports tool approval (auto/manual/conditional) and sub-agents (stateful/stateless modes).
- **`coda_tools`** — Built-in tool implementations and the tool spec system. Provides 10 built-in tools (`shell`, `read_file`, `write_file`, `edit_file`, `ls`, `grep`, `glob`, `read_todos`, `write_todos`, `run_javascript`), `TodoItem`, the `ToolSpec` factory trait (with `name()` metadata), `BuildContext`, `PrebuiltToolSpec`, and name-based resolution (`builtin_specs()`, `spec_by_name`). Depends on `coda_core` and uses `coda_ptc` only for the runner spec.
- **`coda_ptc`** — Provider-independent programmatic tool calling runtime. Runs bounded ES2020 JavaScript in a short-lived rquickjs runtime on a dedicated thread, exposes only a persisted generation-time subset of built-in tools through an async host bridge, and owns console/result limits, watchdogs, and JS execution reports. Depends only on `coda_core` among Coda crates.
- **`coda_skills`** — Loads skill definitions from `.coda/skills/<name>/SKILL.md` directories. Parses YAML frontmatter (name, description, etc.) and generates XML for system-prompt injection.
- **`coda_mcp`** — MCP (Model Context Protocol) client integration. Supports stdio and HTTP (streamable-http) transports, adapts MCP server tools into `ToolObject` instances via `McpToolAdapter`, auto-prefixes tool names with `mcp__` and truncates to 64 chars. Configuration is read from the `mcpServers` field in a JSON file.
- **`coda_server`** — Application layer: axum WebSocket server speaking JSON-RPC 2.0 over a single connection, with live `Session`s owned by the process-level `SessionHub` independently of connections (latest attachment wins per session, and running turns survive disconnects), ask_user tool, a `Transport` trait that receives raw frame text and sends `RpcOutgoing` envelopes, request/notification dispatch, system-prompt construction, file-based agent configuration (loads `.coda/agents/` into a validated `AgentTeam` at startup — see below), tool approval config, MCP server loading, and session persistence (PostgreSQL). Located at `app/coda_server`. The user-facing client is the `coda_web` dashboard (`app/coda_web`).

### Server Configuration

The server reads `coda-server.toml` (overridable via `CODA_SERVER_CONFIG` env var). It declares providers and workspaces:

- **Providers** — `[[providers]]` array-of-tables. Each is an OpenAI-compatible endpoint with `id`, `kind` (`"generic"` or `"deepseek"`), `api_key` / `base_url` (both support `${VAR}` env expansion), and an inline `models` array. Each model has a required `id` (the API model name sent in requests), an optional `name` (human-readable dashboard label; defaults to `id`), a required positive `context_window` token count, optional `reasoning_efforts` (array of arbitrary strings passed to the provider API; `"off"` is reserved for turning thinking off — omit it if the model doesn't support disabling thinking; omit the entire field for non-reasoning models), optional `default_reasoning_effort` (the recommended initial effort level — must be one of `reasoning_efforts`; defaults to the first entry when absent), optional `input_modalities` (list of `"text"` and/or `"image"`; defaults to `["text"]` — add `"image"` to enable image attachments for that model), and optional `auto_compact_threshold` (the token count at which a session on this model automatically compacts context mid-turn; defaults to 80% of `context_window` when absent). Models under the same provider share one `Arc<OpenAI>` instance. The dashboard shows a grouped dropdown (provider → model) and a reasoning-effort selector when the selected model has reasoning levels.
- **Workspaces** — `[[workspaces]]` array-of-tables with `id` and `path`. Sessions are scoped to a workspace; the workspace `id` is part of every session's primary key.
- **Database** — required `[database]` table with a `url` (supports `${VAR}` expansion). PostgreSQL 13 or newer (for the built-in `gen_random_uuid()`). Sessions live in PostgreSQL: `sessions` is the aggregate root, with `thread_checkpoints`, `messages` (one row per message) and `runtime_snapshots` hanging off it via composite foreign keys with `ON DELETE CASCADE`. Tool state lives in `thread_state`, anchored to its recording message; deleting that message cascades to the state, so rewinds cut both together. Migrations are embedded in the binary and run at startup, so deploying creates or updates the schema. Connections are **not** TLS, so the database must be on a trusted network. The storage tests need a live database and are gated behind the `pg-tests` feature (`DATABASE_URL` + `cargo test -p coda_server --features pg-tests --test storage_pg`); point it at a throwaway database, never a real one.

  Queries go through diesel's DSL, type-checked against `app/coda_server/src/schema.rs` — **a generated file; `diesel migration run` rewrites it, never hand-edit**. Typed `jsonb` columns go through `Json<T>` (`src/jsonb.rs`), since diesel only ships `Jsonb` support for `serde_json::Value`. Foreign keys are composite: session-owned rows use `(workspace_id, session_id)`, while `thread_state` uses `(workspace_id, session_id, message_id)` to target its message anchor. Diesel's `joinable!`/`belongs_to` cannot express these, so joins spell out `.on(...)`.
- **Relay** — optional `[relay]` table tuning the process-level session relay's (`coda_server::hub::SessionHub`) per-session in-memory event buffering: `max_log_events` (soft cap on buffered events per turn, default 8192) and `max_message_tier_events` (hard cap on buffered message-tier events per turn, default 4096; exceeding it forces a resync from the persisted state rather than buffering without bound). Both fall back to their default independently when absent.
- **Keepalive** — optional `[keepalive]` table with `interval_secs` (default 30), how often an idle WebSocket connection emits a protocol-level Ping so proxies and load balancers don't cut it. Browsers answer Pings automatically, so this needs nothing from the client — and the client cannot send Pings itself, since the browser WebSocket API exposes no such call. Pings only keep the connection open; nothing tracks Pongs, so a peer that stops answering is noticed only when a write eventually fails.

Selection keys on the wire are composite (`{provider_id}:{model_id}`). The first model of the first provider is the default.

### Workspace Approval Configuration

Each session runs under a **permission mode** (`coda_server::config::PermissionMode`), chosen in the composer. A mode is an allow-list: the tools it names run unattended and everything else suspends for human approval. It is distinct from `coda_agent::ToolApprovalMode` one layer down — that is the runtime's mechanism (`Auto` / `Manual` / `RequireWhen`), this is the user's choice; `ToolApprovalConfig::into_approval_mode` turns one into the other.

| mode | auto-approves |
| --- | --- |
| `explore` | `ls`, `read_file`, `glob`, `grep`, `read_todos`, `write_todos` |
| `accept_edits` (default) | the above plus `write_file`, `edit_file` |
| `yolo` | everything, `shell` included |

Delegation (`agent__*`) is auto-approved under every mode — the sub-agent's own calls go through this same policy, so gating the hand-off too would charge two approvals for one action.

The mode is per session and lives only in memory: `PermissionModeCell` is shared between the hub entry and the approval closure inside the runtime, so `set_permission_mode` takes effect on the next tool call without a rebuild — accepted mid-turn and mid-suspension, and applying to the next call rather than to one already parked. It survives the `SetModel` rebuild and the `Pending` → `Live` promotion because it hangs off the entry, not the phase. Nothing is persisted: the attach that *opens* a session seeds the mode from the client, every later attach is told the live value in its snapshot (a client reconnecting to a running session adopts it), and the web client remembers a mode per session in `localStorage` so a released session reopens as it was.

Workspace rules in `.coda/config.toml` layer on top. `[permissions.tools].approval_required` only ever *tightens*: a tool it matches suspends under every mode, `yolo` included (use `mcp__server__*` to cover one MCP server). It defaults to empty, since the mode now carries the baseline. The `ask_user` tool is always interactive and always suspends to open the web UI.

`[permissions.shell].allow` is the exception that runs the other way — the one place a workspace *grants* something the mode did not. It is a list of specific commands a human vetted and checked into the repo (it is also what the approval panel's "always allow" button writes), and it applies under every mode, so a workspace that allows `cargo test *` gets it unattended even in `explore`.

Shell approvals use `[permissions.shell]` allow/deny glob lists. Outside `yolo`, a `shell` call auto-approves only when every decomposed simple command matches `allow`, no simple command matches `deny`, and the command uses only statically-vetted sequencing/pipe constructs; other shell constructs suspend for approval. Under `yolo` the allow-list is skipped but `deny` still bites — on the commands that decompose, which is what it can be checked against.

### Key Abstractions

- **`LLMProvider`** (`coda_core::llm`) — Model provider trait; core method `stream()` returns `Stream<LLMStreamEvent>`.
- **`Tool` / `ToolObject`** (`coda_core::tool`) — `Tool` is a generic trait (associated types Parameters/Output); `ToolObject` is the object-safe, dynamically-dispatched counterpart. `ToolWrapper` bridges the two.
- **`ThreadState` / `ToolCallContext`** (`coda_core::tool`) — `ThreadState` gives tools durable state on the calling thread, keyed by an opaque `kind`; each `set` is a complete value and the latest write wins. A call reads the batch-start snapshot plus its own writes, while concurrent sibling calls remain isolated. Successful writes are anchored to the tool-result message, so forks copy state with retained anchors and rewinds delete it with discarded anchors.
- **`ToolSpec` / `BuildContext`** (`coda_tools::spec`) — `ToolSpec` is a factory trait for creating tool instances; `BuildContext` carries the workspace directory and file lock registry during tool construction.
- **`KeyedLock`** (`coda_tools::locks`) — mutual exclusion keyed by an arbitrary value; holders of the same key run one at a time. The file tools take it, keyed by canonical path, across their whole read-modify-write, because tool calls run concurrently (several per assistant message, and in parallel across sub-agents and sessions) and two `edit_file` calls on one file would otherwise silently lose an edit. Exclusion only holds between users of the *same* registry, so the registry must be process-wide: `shared_file_locks()` returns that one, and it's the default everywhere (`BuildContext::new`, `SessionBuilder`). Pass your own only to isolate — tests do. Scope is writer-to-writer: the lock is in-process and advisory (a `shell` command or the user's editor writing the same file is out of scope), and readers don't take it, so `read_file`/`grep`/a concurrent `shell` command can still catch a file mid-write, since edits truncate in place. That was judged an acceptable risk; writing to a temp file and renaming is what would close it. **Hold at most one key at a time**; two keys must be acquired in sorted order.
- **`AgentSpec` / `AgentTeam`** (`coda_agent::spec`) — `AgentSpec` is plain per-agent data (sub-agents referenced by name). `AgentTeam::new(root, subagents)` validates the whole set once (unique names, resolvable references, tool/sub-agent namespace conflicts; sub-agents unreachable from the root are dropped; each retained sub-agent's `agent__`-prefixed tool name must fit the 64-character provider limit) so holding one proves it sound; `AgentTeam::build(workspace_dir)` then constructs fresh `Agent`s per session (infallibly).
- **`Session`** (`coda_agent::session`) — High-level API for callers: send tasks, consume events, resume from suspension, and shut down sessions. `SessionBuilder::team(&AgentTeam, workspace_dir)` borrows the team and builds the agents at `open()`.

### Agent Configuration (file-based)

Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`: YAML frontmatter (`description`, `mode` = stateful/stateless, `tools`, `subagents`, `workspace`, `model`, `reasoning_effort`) plus a markdown body used as the system prompt. They become sub-agents of the top-level `coda` agent and may reference one another by name to form deeper graphs (sharing allowed).

The runtime exposes sub-agents to the LLM as `agent__<name>` tools and strips the prefix for routing. The prefixed name is preserved in live events and session history so clients can identify sub-agent invocations directly. Each reachable file-based agent name may contain up to 57 characters, leaving room for the `agent__` prefix within the 64-character provider limit.

Agents may optionally override the session’s model via the `model` frontmatter field, a `{provider_id}:{model_id}` selection key (optionally paired with `reasoning_effort`). Overrides are validated against the provider catalog at startup — an unknown model or unsupported reasoning effort is a hard error. When a sub-agent omits `model`, it inherits the session’s default (root) model.

Each agent's system prompt is a single template — the **base** body (the `AGENT.md` body / built-in `system-prompt.md`; read once at workspace load) — run through a `{{name}}` variable-substitution pass each turn. Everything dynamic is a binding the body composes: `date`, `os`, `shell`, `workspace` (environment context); `skills_guide` (the constant skills usage guide) and `workspace_available_skills` (the workspace's `<available_skills>` XML, empty when none); and `workspace_custom_instructions` (the workspace's `AGENTS.md`, wrapped in `<custom_instructions>`, empty when none). All bindings are always available — reference the ones you want; unreferenced ones simply don't appear, and unknown `{{placeholders}}` are left untouched. Substitution is **single-pass**: a binding's value is never re-scanned, so authored content (`AGENTS.md`, a skill description) is never itself treated as a template even when it contains `{{…}}`. Only the date is recomputed each turn; the OS/shell/skills-guide are fixed once, and the skills-list and custom-instructions bindings are read from per-workspace handles a watcher refreshes in place (so `AGENTS.md` and skills still hot-reload). The built-in `system-prompt.md` references all of these — an `<environment_context>` block plus the skills and custom-instructions variables — so a bare `coda` agent behaves as before; a custom `AGENT.md` body opts into whichever bindings it wants.

The `workspace:` frontmatter roots an agent at its own directory (its tool root and knowledge source) — absolute, or relative to the root workspace; absent inherits the root (session) workspace. A `workspace:` that doesn't resolve to an existing directory is a hard startup error. Agents sharing a workspace share that workspace's knowledge handles (skills + custom instructions) + watcher. This is a default cwd/relative-root, **not** a sandbox — tools can still reach outside it. A per-agent workspace does **not** load its own `.coda/agents` (agent topology is defined only in the root workspace). Only the **workspace knowledge** (`AGENTS.md` + skills) hot-reloads, via the watcher; agent bodies and all frontmatter (`tools`/`subagents`/`mode`/`model`/`workspace`) are read once at load and need a restart to change. MCP tools and the approval policy remain rooted at the session workspace / session-global.

The `coda` agent itself is configured by an optional `.coda/agents/AGENT.md` (a bare file, not a directory): its `tools`, `subagents`, and body each *explicitly override* a default when present (otherwise: all tools, the auto-attached unreferenced agents, and the built-in `system-prompt.md` base prompt). `coda` is always present.

Tools resolve by name against built-ins plus prebuilt tools (e.g. MCP tools from `mcp.json`). A name ending in `*` is a prefix pattern — `mcp__example__*` enables every tool that server exposes; a bare `*` is not a wildcard. To grant every tool, omit `tools` on the root `coda` agent (whose default is all tools) — a sub-agent that omits `tools` gets none. Unknown plain tool names, duplicate agent names, dangling sub-agent references, and tool/sub-agent namespace conflicts are hard startup errors; a pattern that matches nothing only warns. Sub-agents unreachable from `coda` are ignored with a warning.
