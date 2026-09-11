# Configuration examples

A self-contained, annotated example of everything coda can be configured with:
providers, workspaces, a multi-agent team, MCP servers, tool approval rules,
skills, workspace knowledge, and per-agent workspaces.

Nothing here is wired into the build — it's reference material. Copy a file into
your own workspace and adapt it.

## Layout

```
examples/
├── coda-server.toml                 # providers + workspaces (point CODA_SERVER_CONFIG here)
└── workspace/                       # the "demo" workspace a session is scoped to
    ├── AGENTS.md                    # root workspace knowledge (custom instructions)
    ├── docs/                        # a per-agent workspace (see docs-writer)
    │   ├── AGENTS.md                #   its own knowledge, separate from the root's
    │   └── .coda/skills/changelog/  #   its own skill (per-workspace, not shared)
    └── .coda/
        ├── mcp.json                 # MCP servers, shared by all agents
        ├── config.toml              # tool approval + shell allow/deny rules
        ├── skills/code-review/      # a root-workspace skill
        └── agents/                  # the agent team (topology lives only here)
            ├── AGENT.md             #   root `coda` config (bare file)
            ├── planner/AGENT.md     #   orchestrator: delegates to researcher + coder
            ├── researcher/AGENT.md  #   read-only, cheap model, [date]-only env
            ├── coder/AGENT.md       #   read/write/shell, reasoning model, full env
            └── docs-writer/AGENT.md #   rooted at ./docs (per-agent workspace)
```

## Run it

```sh
# The ${VAR} keys in coda-server.toml must be set (any value loads the example —
# model validation checks the catalog, not connectivity):
export DEEPSEEK_API_KEY=dummy OPENROUTER_API_KEY=dummy

CODA_SERVER_CONFIG=examples/coda-server.toml \
  cargo run -p coda_server --bin coda-server
```

The workspace `path` in `coda-server.toml` is resolved relative to that config
file's own directory, so `workspace` points at `examples/workspace` no matter
where you launch from.

## What each piece demonstrates

| File | Capability |
| --- | --- |
| `coda-server.toml` | Multiple providers, per-model `context_window` / `reasoning_efforts` / `input_modalities`, `${VAR}` expansion, workspace declaration. |
| `workspace/AGENTS.md` | Root **workspace knowledge**, exposed to bodies as the `{{workspace_custom_instructions}}` variable and hot-reloaded on change. |
| `.coda/agents/AGENT.md` | Root `coda` overrides: an exclude-only `tools` rule (missing include keeps the all-ordinary-tools default), explicit `subagents`, plus a custom body that composes the env, `{{skills_guide}}`/`{{workspace_available_skills}}`, and `{{workspace_custom_instructions}}` variables. |
| `planner/` | A **stateful** orchestrator with a minimal tool set (`read_todos`/`write_todos`), `capabilities: []`, and its own `subagents` — a deeper graph under `coda`. A pure delegator, so its body pulls in the env and custom instructions but **not** the skills variables. |
| `researcher/` | **Stateless**, read-only tools, only the `ptc` capability, an MCP **prefix pattern** (`mcp__time__*`), a per-agent **model override** to a cheaper model, and a body that references the skills variables. |
| `coder/` | A fuller tool set incl. `shell`, default capabilities (`background` and `ptc`), the full env block plus `{{skills_guide}}`/`{{workspace_available_skills}}`, a **reasoning model** override with `reasoning_effort: high`. |
| `docs-writer/` | A **per-agent workspace** (`workspace: ./docs`): its tool root and knowledge come from `docs/`, so its `{{workspace_available_skills}}`/`{{workspace_custom_instructions}}` resolve against `docs/`, not the root. |
| `docs/AGENTS.md` + `docs/.coda/skills/` | A per-agent workspace carries **its own** knowledge and skills, distinct from the root's. |
| `.coda/mcp.json` | An MCP server over **stdio** (`mcp-server-time` via `uvx`); referenced from agents as `mcp__<server>__<tool>`. An **http** server uses `{ "type": "http", "url": ... }` instead. |
| `.coda/config.toml` | Workspace approval rules layered on the session's permission mode: `approval_required` tool-name patterns (tightening only — they hold under `yolo` too) plus `shell` `allow`/`deny` globs. `ask_user` always pauses to open the UI. |
| `.coda/skills/code-review/` | A **skill** — name + description frontmatter plus a body, surfaced to agents in this workspace. |

## Notes on the model

- **Agent topology lives only in the root workspace.** A per-agent workspace
  (like `docs/`) is *not* scanned for its own `.coda/agents` — only its
  `AGENTS.md` and `.coda/skills`.
- A per-agent workspace is a **default cwd, not a sandbox**: tools can still
  reach outside it with an explicit path.
- **Workspace knowledge hot-reloads.** Editing workspace `AGENTS.md` or a
  skill is picked up on the next turn. Agent bodies and frontmatter
  (`tools`, `capabilities`, `subagents`, `mode`, `model`, `workspace`) need a restart.
- **Tools and capabilities have independent defaults for every agent.** Omit
  `tools` to get all registered ordinary tools (including `ask_user` and MCP),
  or provide a list / `include` to replace that set. `exclude` subtracts from
  it; with only `exclude`, the starting set is all ordinary tools. Omit
  `capabilities` to enable all supported capabilities (`background`, `ptc`),
  or use a list to select them. An empty list disables that category.
  A caller's choices do not change its children's configuration.
- **Capability tools are configured through capabilities.** Remove
  `run_javascript` from old ordinary tool lists; PTC now comes from `ptc`.
  `run_javascript`, `list_javascript_tools`, `task_output`, and `task_kill`
  are reserved names, rejected in ordinary tool lists and as sub-agent names.
- **MCP and approval are workspace-/session-wide**, not per-agent: `mcp.json`
  and `config.toml` are loaded once from the root workspace and shared.
- **The permission mode is per session**, picked in the composer
  (`explore` / `accept_edits` / `yolo`) and switchable at any time — including
  mid-turn. It is the allow-list; `config.toml` can only tighten it further.
- An **unreachable MCP server is skipped** with a warning at startup, not fatal —
  so a down server never blocks the session.
