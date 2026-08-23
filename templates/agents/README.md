# Agent System Prompt Templates

Reusable system-prompt bodies for common agent scenarios. Each file here is a starting point, not something the server loads directly — copy it into place under a workspace's `.coda/agents/` and adjust the frontmatter for that agent.

## Using a template

- **As the root `coda` agent** — copy the file to `.coda/agents/AGENT.md` (a bare file, not a directory). The root agent only reads the `tools:` and `subagents:` frontmatter fields; drop the rest (`description`, `mode`, etc.) down to an empty `---\n---` block, since the root agent doesn't need them.
- **As a named sub-agent** — copy the file to `.coda/agents/<name>/AGENT.md` and fill in `description` and `mode` (`stateful` or `stateless`) at minimum. Add `tools`, `subagents`, `workspace`, `model`, and `reasoning_effort` as needed — see `AGENTS.md` at the repo root ("Agent Configuration (file-based)") for what each field means.

Every template's body can use the same `{{...}}` bindings as the built-in `app/coda_server/src/system-prompt.md`: `{{date}}`, `{{os}}`, `{{shell}}`, `{{workspace}}`, `{{skills_guide}}`, `{{workspace_available_skills}}`, and `{{workspace_custom_instructions}}`.

## Available templates

- [`coding-agent.md`](coding-agent.md) — general-purpose agent for reading, writing, debugging, and reasoning about code in a real workspace.
