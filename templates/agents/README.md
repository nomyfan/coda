# Agent System Prompt Templates

Reusable system-prompt bodies for common agent scenarios. Each file here is a starting point, not something the server loads directly — copy it into place under a workspace's `.coda/agents/` and adjust the frontmatter for that agent.

## Using a template

- **As the root `coda` agent** — copy the file to `.coda/agents/AGENT.md` (a bare file, not a directory). The root agent reads the `tools:`, `capabilities:`, and `subagents:` frontmatter fields; drop the rest (`description`, `mode`, etc.). An empty `---\n---` block uses the defaults.
- **As a named sub-agent** — copy the file to `.coda/agents/<name>/AGENT.md` and fill in `description` and `mode` (`stateful` or `stateless`) at minimum. Add `tools`, `capabilities`, `subagents`, `workspace`, `model`, and `reasoning_effort` as needed — see `AGENTS.md` at the repo root ("Agent Configuration (file-based)") for what each field means.

Root and sub-agents default to all registered ordinary tools and all supported capabilities (`background`, `ptc`). Each agent configures these independently of its callers. Explicit lists replace the defaults; `tools: []` removes ordinary tools, while `capabilities: []` disables capabilities. Neither changes the agent's `subagents` list. A tool rule with only `exclude` starts from all ordinary tools.

For example, add these fields for an agent that can read files and use PTC:

```yaml
tools: [read_file, ls]
capabilities: [ptc]
```

Configure `run_javascript`, `list_javascript_tools`, `task_output`, and `task_kill` through capabilities; these reserved names are rejected in ordinary tool lists and as sub-agent names. PTC also needs eligible tools allowed by the current approval policy; background execution needs the session's task registry. Agent bodies and frontmatter are loaded at startup, so changes need a restart.

Every template's body can use the same `{{...}}` bindings as the built-in `app/coda_server/src/system-prompt.md`: `{{date}}`, `{{os}}`, `{{shell}}`, `{{workspace}}`, `{{skills_guide}}`, `{{workspace_available_skills}}`, and `{{workspace_custom_instructions}}`.

## Available templates

- [`coding-agent.md`](coding-agent.md) — general-purpose agent for reading, writing, debugging, and reasoning about code in a real workspace.
