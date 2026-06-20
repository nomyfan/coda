//! File-based agent definitions.
//!
//! Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`:
//! YAML frontmatter (description, mode, tools, subagents, env, workspace, model)
//! followed by a markdown body used as the agent's system prompt. They become
//! sub-agents of the top-level `coda` agent and may reference one another by name
//! to form deeper graphs.
//!
//! The top-level `coda` agent itself is configured by an optional
//! `.coda/agents/AGENT.md` (a bare file, distinct from the per-agent directories).
//! Its `tools`, `subagents`, and body each *explicitly override* a default when
//! present; otherwise the built-ins apply (all tools, auto-attached unreferenced
//! agents, and the built-in base prompt respectively). `coda` is always present.
//!
//! A [`ToolRegistry`] resolves the `tools` list: built-in tools by name, plus
//! any prebuilt tools (MCP, `ask_user`) registered at startup. A name ending in
//! `*` over a non-empty prefix is a pattern (e.g. `mcp__example__*` enables
//! every tool that server exposes); a bare `*` is *not* a wildcard. To grant
//! every tool, omit `tools` on the root `coda` agent (its default is all tools)
//! — a sub-agent that omits `tools` gets none. An unknown plain name is a hard
//! error, surfaced at startup; a pattern that matches nothing only warns.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use coda_agent::{
    AgentSpec, AgentTeam, BuildError, SharedSystemPrompt, SubAgentMode, SystemPrompt,
};
use coda_core::llm::ReasoningEffort;

use crate::{EnvField, default_env_fields, make_env_renderer};
use coda_core::tool::ToolObject;
use coda_tools::{BUILTIN_TOOL_NAMES, PrebuiltToolSpec, ToolSpec, builtin_specs, spec_by_name};
use serde::Deserialize;
use tracing::{info, warn};

/// The top-level agent's name. Reserved: configured agents may neither use it
/// nor reference it as a sub-agent.
pub const ROOT_AGENT_NAME: &str = "coda";

const AGENTS_SUBDIR: &str = "agents";
const AGENT_FILE: &str = "AGENT.md";

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    /// A specific agent file failed to parse. Carries the agent name and reason.
    Parse {
        agent: String,
        reason: String,
    },
    /// An agent directory name is not a valid agent name.
    InvalidName(String),
    /// An agent file is named (or references) the reserved root name.
    ReservedName(String),
    /// An agent's `tools` list names a tool that is neither built-in nor a
    /// registered prebuilt (MCP / `ask_user`) tool.
    UnknownTool {
        agent: String,
        tool: String,
    },
    /// An agent's `workspace:` does not resolve to an existing directory.
    InvalidWorkspace {
        agent: String,
        path: String,
        reason: String,
    },
    /// The assembled team failed structural validation: duplicate names,
    /// dangling sub-agent references, or tool/sub-agent namespace conflicts.
    Build(BuildError),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "agent config I/O error: {e}"),
            LoadError::Parse { agent, reason } => {
                write!(f, "failed to parse agent '{agent}': {reason}")
            }
            LoadError::InvalidName(name) => write!(
                f,
                "invalid agent name '{name}': use lowercase letters, digits and hyphens"
            ),
            LoadError::ReservedName(name) => {
                write!(f, "'{name}' is reserved for the top-level agent")
            }
            LoadError::UnknownTool { agent, tool } => {
                write!(f, "agent '{agent}' requests unknown tool '{tool}'")
            }
            LoadError::InvalidWorkspace {
                agent,
                path,
                reason,
            } => write!(f, "agent '{agent}' workspace '{path}' is invalid: {reason}"),
            LoadError::Build(e) => write!(f, "invalid agent configuration: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

/// Resolves tool names to [`ToolSpec`] factories. Built-in tools are resolved by
/// name; prebuilt tools (MCP adapters, `ask_user`) are registered explicitly and
/// shared across every agent that names them.
#[derive(Default)]
pub struct ToolRegistry {
    prebuilt: BTreeMap<String, PrebuiltToolSpec>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a prebuilt tool under its own `name()`. Replaces any tool
    /// previously registered under the same name.
    pub fn insert(&mut self, tool: Box<dyn ToolObject>) {
        self.prebuilt
            .insert(tool.name().to_string(), PrebuiltToolSpec::new(tool));
    }

    /// Resolve a tool name to a fresh spec, or `None` if unknown.
    fn resolve(&self, name: &str) -> Option<Box<dyn ToolSpec>> {
        if let Some(spec) = spec_by_name(name) {
            return Some(spec);
        }
        self.prebuilt
            .get(name)
            .map(|p| Box::new(p.clone()) as Box<dyn ToolSpec>)
    }

    /// Expand a trailing-`*` prefix pattern (e.g. `mcp__example__*`) to every tool
    /// — built-in or prebuilt — whose name starts with `prefix`, as fresh specs
    /// sorted by name. `prefix` is always non-empty (the caller rejects a bare
    /// `*`).
    fn expand_pattern(&self, prefix: &str) -> Vec<Box<dyn ToolSpec>> {
        let mut names: Vec<&str> = BUILTIN_TOOL_NAMES
            .iter()
            .copied()
            .chain(self.prebuilt.keys().map(String::as_str))
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort_unstable();
        names
            .iter()
            .map(|name| {
                // Every name came straight from a known source, so a miss is a
                // broken internal invariant — surface it rather than dropping it.
                self.resolve(name).expect("enumerated tool name resolves")
            })
            .collect()
    }

    /// Every registered prebuilt tool, for handing to the top-level agent.
    fn all_prebuilt(&self) -> Vec<Box<dyn ToolSpec>> {
        self.prebuilt
            .values()
            .map(|p| Box::new(p.clone()) as Box<dyn ToolSpec>)
            .collect()
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ModeRaw {
    Stateful,
    Stateless,
}

impl From<ModeRaw> for SubAgentMode {
    fn from(m: ModeRaw) -> Self {
        match m {
            ModeRaw::Stateful => SubAgentMode::Stateful,
            ModeRaw::Stateless => SubAgentMode::Stateless,
        }
    }
}

#[derive(Deserialize)]
struct Frontmatter {
    description: String,
    mode: ModeRaw,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    subagents: Vec<String>,
    /// Optional environment-context fields. Absent means the default ([`date`]).
    #[serde(default)]
    env: Option<Vec<EnvField>>,
    /// Optional per-agent workspace (tool root + knowledge source). Absolute, or
    /// relative to the root workspace. Absent means inherit the root workspace.
    #[serde(default)]
    workspace: Option<String>,
    /// Optional model override, as a `{provider_id}:{model_id}` selection key.
    /// Absent means the agent inherits the session's default (root) model.
    #[serde(default)]
    model: Option<String>,
    /// Optional reasoning effort for the overridden model. Validated against the
    /// model's configured levels at startup.
    #[serde(default)]
    reasoning_effort: Option<ReasoningEffort>,
}

/// A parsed agent file (before tool resolution).
pub struct AgentFile {
    name: String,
    description: String,
    mode: SubAgentMode,
    tools: Vec<String>,
    subagents: Vec<String>,
    system_prompt: String,
    env: Vec<EnvField>,
    workspace: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
}

impl AgentFile {
    /// The agent's name (its directory name under `.coda/agents/`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The raw `workspace:` frontmatter value, if any (unresolved).
    pub fn workspace(&self) -> Option<&str> {
        self.workspace.as_deref()
    }

    /// The configured model selection key (`{provider_id}:{model_id}`), if any.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// The configured reasoning effort for the overridden model, if any.
    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }
}

/// Frontmatter of the optional top-level `.coda/agents/AGENT.md`. Both fields are
/// `Option` so "absent" (use the default) is distinct from an explicit empty
/// list (override to nothing).
#[derive(Deserialize, Default)]
struct RootFrontmatter {
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    subagents: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<EnvField>>,
}

/// Parsed top-level `coda` configuration. Each field is an explicit override of a
/// default when `Some`/non-empty; otherwise the built-in behavior applies. See
/// [`build_agent_team`] (tools, sub-agents) and the system-prompt assembly (body).
pub struct RootAgentFile {
    pub tools: Option<Vec<String>>,
    pub subagents: Option<Vec<String>>,
    /// The body, used as the root agent's base system prompt; `None` when the
    /// file is absent or its body is empty (fall back to the built-in default).
    pub system_prompt: Option<String>,
    /// Environment-context fields for the root agent. Defaults to [`date`] when
    /// the file is absent or omits `env:`.
    pub env: Vec<EnvField>,
}

impl Default for RootAgentFile {
    fn default() -> Self {
        RootAgentFile {
            tools: None,
            subagents: None,
            system_prompt: None,
            env: default_env_fields(),
        }
    }
}

/// True if `name` is a syntactically valid agent name (lowercase alphanumerics
/// and hyphens, not starting or ending with a hyphen).
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Split YAML frontmatter from the markdown body. Mirrors the skills parser.
fn split_frontmatter(content: &str) -> Result<(&str, &str), String> {
    if !content.starts_with("---") {
        return Err("file must start with YAML frontmatter (---)".to_string());
    }
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    if parts.len() < 3 {
        return Err("frontmatter not closed with ---".to_string());
    }
    Ok((parts[1], parts[2].trim()))
}

fn parse_agent_file(name: &str, content: &str) -> Result<AgentFile, LoadError> {
    let parse_err = |reason: String| LoadError::Parse {
        agent: name.to_string(),
        reason,
    };

    let (frontmatter, body) = split_frontmatter(content).map_err(parse_err)?;
    let fm: Frontmatter = serde_yml::from_str(frontmatter)
        .map_err(|e| parse_err(format!("invalid frontmatter: {e}")))?;

    if body.is_empty() {
        return Err(parse_err("system prompt (file body) is empty".to_string()));
    }
    if let Some(reserved) = fm.subagents.iter().find(|s| *s == ROOT_AGENT_NAME) {
        return Err(LoadError::ReservedName(reserved.clone()));
    }

    Ok(AgentFile {
        name: name.to_string(),
        description: fm.description,
        mode: fm.mode.into(),
        tools: fm.tools,
        subagents: fm.subagents,
        system_prompt: body.to_string(),
        env: fm.env.unwrap_or_else(default_env_fields),
        workspace: fm.workspace,
        model: fm.model,
        reasoning_effort: fm.reasoning_effort,
    })
}

/// Load every `.coda/agents/<name>/AGENT.md` under `workspace_dir`. Returns an
/// empty list when the directory is absent. Agents are sorted by name for a
/// deterministic build.
pub fn load_agent_files(workspace_dir: &Path) -> Result<Vec<AgentFile>, LoadError> {
    let dir = workspace_dir.join(".coda").join(AGENTS_SUBDIR);
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let agent_md = path.join(AGENT_FILE);
        if !agent_md.exists() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if name == ROOT_AGENT_NAME {
            return Err(LoadError::ReservedName(name));
        }
        if !is_valid_name(&name) {
            return Err(LoadError::InvalidName(name));
        }
        let content = std::fs::read_to_string(&agent_md)?;
        files.push(parse_agent_file(&name, &content)?);
    }

    files.sort_by(|a, b| a.name.cmp(&b.name));
    info!("loaded {} configured agent(s)", files.len());
    Ok(files)
}

fn parse_root_agent_file(content: &str) -> Result<RootAgentFile, LoadError> {
    let parse_err = |reason: String| LoadError::Parse {
        agent: ROOT_AGENT_NAME.to_string(),
        reason,
    };

    let (frontmatter, body) = split_frontmatter(content).map_err(parse_err)?;
    let fm: RootFrontmatter = serde_yml::from_str(frontmatter)
        .map_err(|e| parse_err(format!("invalid frontmatter: {e}")))?;

    if let Some(reserved) = fm
        .subagents
        .iter()
        .flatten()
        .find(|s| *s == ROOT_AGENT_NAME)
    {
        return Err(LoadError::ReservedName(reserved.clone()));
    }

    Ok(RootAgentFile {
        tools: fm.tools,
        subagents: fm.subagents,
        system_prompt: (!body.is_empty()).then(|| body.to_string()),
        env: fm.env.unwrap_or_else(default_env_fields),
    })
}

/// Load the optional top-level `.coda/agents/AGENT.md` that configures the `coda`
/// agent itself. Returns all-default ([`RootAgentFile::default`]) when absent.
pub fn load_root_agent_file(workspace_dir: &Path) -> Result<RootAgentFile, LoadError> {
    let path = workspace_dir
        .join(".coda")
        .join(AGENTS_SUBDIR)
        .join(AGENT_FILE);
    if !path.exists() {
        return Ok(RootAgentFile::default());
    }
    let content = std::fs::read_to_string(&path)?;
    info!("loaded top-level {AGENT_FILE} for '{ROOT_AGENT_NAME}'");
    parse_root_agent_file(&content)
}

/// Resolve a list of tool names to [`ToolSpec`] factories via `registry`.
///
/// A name ending in `*` with a non-empty prefix is a pattern (e.g.
/// `mcp__example__*`), expanded to every matching tool; a pattern that matches
/// nothing is warned about, not an error, since which MCP tools exist can
/// legitimately vary. A bare `*` is not a pattern (to grant every tool, omit
/// `tools` on the root agent) — it falls through and resolves to nothing, a hard
/// error. A plain name that resolves to nothing is likewise a hard
/// [`LoadError::UnknownTool`] (attributed to `agent`). The result is
/// deduplicated by name so a pattern overlapping a literal entry doesn't trip
/// the tool-namespace conflict check downstream.
fn resolve_tools(
    registry: &ToolRegistry,
    agent: &str,
    names: &[String],
) -> Result<Vec<Box<dyn ToolSpec>>, LoadError> {
    let mut tools: Vec<Box<dyn ToolSpec>> = Vec::with_capacity(names.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |tools: &mut Vec<Box<dyn ToolSpec>>, spec: Box<dyn ToolSpec>| {
        if seen.insert(spec.name().to_string()) {
            tools.push(spec);
        }
    };

    for name in names {
        // A trailing `*` over a non-empty prefix is a pattern; a bare `*` is
        // not (drop the whole `tools` field to get every tool) and falls
        // through to the literal path, where it resolves to nothing.
        if let Some(prefix) = name.strip_suffix('*').filter(|p| !p.is_empty()) {
            let matches = registry.expand_pattern(prefix);
            if matches.is_empty() {
                warn!(agent, pattern = name, "tool pattern matched no tools");
            }
            for spec in matches {
                push(&mut tools, spec);
            }
        } else {
            match registry.resolve(name) {
                Some(spec) => push(&mut tools, spec),
                None => {
                    return Err(LoadError::UnknownTool {
                        agent: agent.to_string(),
                        tool: name.clone(),
                    });
                }
            }
        }
    }
    Ok(tools)
}

/// Assemble a validated [`AgentTeam`] rooted at the top-level `coda` agent.
///
/// `root_tools` / `root_subagents` come from the optional `.coda/agents/AGENT.md`
/// and each *explicitly override* a default when present:
/// - tools default to all built-ins + every prebuilt tool;
/// - direct sub-agents default to the configured agents that no *other* agent
///   references (self-references don't count, so a self-loop still attaches).
///
/// Fallible for unknown tool names ([`LoadError::UnknownTool`]) and for any
/// structural problem [`AgentTeam::new`] rejects — duplicate names, dangling
/// references, or tool/sub-agent conflicts ([`LoadError::Build`]).
/// Agents unreachable from `coda` are ignored with a warning.
///
/// Every agent's system prompt — root and sub alike — is assembled here from
/// three segments: a base body handle, the workspace-knowledge handle for the
/// agent's own workspace (looked up in `knowledge`), and a per-turn env block
/// rendered against that workspace. Each agent's tool root is recorded on the
/// returned team via [`AgentTeam::with_agent_workspaces`]. `agent_workspaces`
/// maps sub-agent names to their resolved workspace; an agent absent there (and
/// the root) uses `root_workspace`.
#[allow(clippy::too_many_arguments)]
pub fn build_agent_team(
    root_workspace: &str,
    root_base: SharedSystemPrompt,
    root_env: Vec<EnvField>,
    knowledge: &HashMap<String, SharedSystemPrompt>,
    agent_workspaces: &HashMap<String, String>,
    registry: &ToolRegistry,
    files: Vec<AgentFile>,
    root_tools: Option<Vec<String>>,
    root_subagents: Option<Vec<String>>,
) -> Result<AgentTeam, LoadError> {
    // Assemble a three-segment prompt for an agent rooted at `workspace`: base
    // body, that workspace's knowledge handle (if any), and a per-turn env block.
    let assemble = |base: SharedSystemPrompt, workspace: &str, env: Vec<EnvField>| {
        let mut prompt = SystemPrompt::new(base);
        if let Some(handle) = knowledge.get(workspace) {
            prompt = prompt.with_workspace_knowledge(handle.clone());
        }
        if let Some(renderer) = make_env_renderer(workspace.to_string(), env) {
            prompt = prompt.with_env(renderer);
        }
        prompt
    };

    let roots = match root_subagents {
        Some(explicit) => explicit,
        None => {
            let referenced: HashSet<&str> = files
                .iter()
                .flat_map(|f| {
                    f.subagents
                        .iter()
                        .map(String::as_str)
                        .filter(move |&child| child != f.name.as_str())
                })
                .collect();
            files
                .iter()
                .filter(|f| !referenced.contains(f.name.as_str()))
                .map(|f| f.name.clone())
                .collect()
        }
    };

    let root_tools = match root_tools {
        Some(names) => resolve_tools(registry, ROOT_AGENT_NAME, &names)?,
        None => {
            let mut tools = builtin_specs();
            tools.extend(registry.all_prebuilt());
            tools
        }
    };

    let root = AgentSpec {
        name: ROOT_AGENT_NAME.to_string(),
        description: String::new(),
        system_prompt: assemble(root_base, root_workspace, root_env),
        mode: SubAgentMode::Stateful,
        tools: root_tools,
        subagents: roots,
    };

    let mut subagents = Vec::with_capacity(files.len());
    for file in files {
        let tools = resolve_tools(registry, &file.name, &file.tools)?;
        let workspace = agent_workspaces
            .get(&file.name)
            .map(String::as_str)
            .unwrap_or(root_workspace);
        let system_prompt = assemble(
            SharedSystemPrompt::new(file.system_prompt),
            workspace,
            file.env,
        );
        subagents.push(AgentSpec {
            name: file.name,
            description: file.description,
            system_prompt,
            mode: file.mode,
            tools,
            subagents: file.subagents,
        });
    }

    // The full name → workspace lookup the team roots tools through; the root is
    // just another entry, not a special case.
    let mut workspaces = agent_workspaces.clone();
    workspaces.insert(ROOT_AGENT_NAME.to_string(), root_workspace.to_string());

    AgentTeam::new(root, subagents)
        .map(|team| team.with_agent_workspaces(workspaces))
        .map_err(LoadError::Build)
}

/// Resolve an agent's `workspace:` frontmatter to an absolute, existing
/// directory. A relative path is taken against `root_workspace`; an absent value
/// inherits `root_workspace` itself. A path that does not resolve to an existing
/// directory is a hard error — an agent must never silently root at the wrong
/// place.
pub fn resolve_agent_workspace(
    agent: &str,
    root_workspace: &str,
    raw: Option<&str>,
) -> Result<String, LoadError> {
    let Some(raw) = raw else {
        return Ok(root_workspace.to_string());
    };

    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Path::new(root_workspace).join(path)
    };

    // Report the joined path (what was actually looked up), noting the raw value
    // a relative `./sub` is otherwise ambiguous without its resolution base.
    let invalid = |reason: String| LoadError::InvalidWorkspace {
        agent: agent.to_string(),
        path: joined.to_string_lossy().into_owned(),
        reason: format!("{reason} (from workspace: '{raw}')"),
    };

    let canonical = joined.canonicalize().map_err(|e| invalid(e.to_string()))?;
    if !canonical.is_dir() {
        return Err(invalid("not a directory".to_string()));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;

    use coda_core::tool::ToolResult;

    use super::*;

    /// A bare prebuilt tool with a fixed name, standing in for an MCP tool.
    struct FakeTool {
        name: String,
        schema: serde_json::Value,
    }

    impl FakeTool {
        fn boxed(name: &str) -> Box<dyn ToolObject> {
            Box::new(FakeTool {
                name: name.to_string(),
                schema: serde_json::json!({}),
            })
        }
    }

    impl ToolObject for FakeTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "fake"
        }
        fn parameter_schema(&self) -> &serde_json::Value {
            &self.schema
        }
        fn execute(
            self: Arc<Self>,
            _params: String,
        ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// A registry preloaded with two `mcp__example__*` tools and one other.
    fn registry_with_mcp() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.insert(FakeTool::boxed("mcp__example__search"));
        registry.insert(FakeTool::boxed("mcp__example__extract"));
        registry.insert(FakeTool::boxed("mcp__other__list"));
        registry
    }

    fn write_agent(dir: &Path, name: &str, content: &str) {
        let agent_dir = dir.join(".coda").join("agents").join(name);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();
    }

    fn write_root_agent(dir: &Path, content: &str) {
        let agents_dir = dir.join(".coda").join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("AGENT.md"), content).unwrap();
    }

    /// Build a team rooted at `/ws` with no per-agent workspaces or knowledge —
    /// enough for the structural/tool-resolution tests below.
    fn build_team(
        registry: &ToolRegistry,
        files: Vec<AgentFile>,
        root_tools: Option<Vec<String>>,
        root_subagents: Option<Vec<String>>,
    ) -> Result<AgentTeam, LoadError> {
        build_agent_team(
            "/ws",
            SharedSystemPrompt::new("root"),
            default_env_fields(),
            &HashMap::new(),
            &HashMap::new(),
            registry,
            files,
            root_tools,
            root_subagents,
        )
    }

    #[test]
    fn no_config_dir_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_agent_files(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn parses_workspace_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "scoped",
            "---\ndescription: x\nmode: stateless\nworkspace: ./sub\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert_eq!(files[0].workspace(), Some("./sub"));
    }

    #[test]
    fn resolve_workspace_absent_inherits_root() {
        let root = tempfile::tempdir().unwrap();
        let root_str = root.path().to_string_lossy();
        assert_eq!(
            resolve_agent_workspace("a", &root_str, None).unwrap(),
            *root_str
        );
    }

    #[test]
    fn resolve_workspace_relative_joins_root() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let root_str = root.path().to_string_lossy();
        let resolved = resolve_agent_workspace("a", &root_str, Some("sub")).unwrap();
        assert_eq!(resolved, sub.canonicalize().unwrap().to_string_lossy());
    }

    #[test]
    fn resolve_workspace_absolute_is_used() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_str = other.path().to_string_lossy().into_owned();
        let resolved =
            resolve_agent_workspace("a", &root.path().to_string_lossy(), Some(&other_str)).unwrap();
        assert_eq!(
            resolved,
            other.path().canonicalize().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn resolve_workspace_missing_dir_errors() {
        let root = tempfile::tempdir().unwrap();
        let result = resolve_agent_workspace("a", &root.path().to_string_lossy(), Some("nope"));
        assert!(matches!(result, Err(LoadError::InvalidWorkspace { .. })));
    }

    #[test]
    fn resolve_workspace_file_not_dir_errors() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), "x").unwrap();
        let result = resolve_agent_workspace("a", &root.path().to_string_lossy(), Some("file.txt"));
        assert!(matches!(result, Err(LoadError::InvalidWorkspace { .. })));
    }

    #[test]
    fn parses_valid_agent() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "explore",
            "---\ndescription: explores\nmode: stateless\ntools: [read_file, grep]\n---\nYou explore.",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "explore");
        assert_eq!(files[0].mode, SubAgentMode::Stateless);
        assert_eq!(files[0].tools, vec!["read_file", "grep"]);
        assert_eq!(files[0].system_prompt, "You explore.");
        // Omitting `env:` defaults to date only.
        assert_eq!(files[0].env, vec![EnvField::Date]);
    }

    #[test]
    fn parses_explicit_env_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "coder",
            "---\ndescription: codes\nmode: stateful\nenv: [date, system, shell, workspace]\n---\nYou code.",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert_eq!(
            files[0].env,
            vec![
                EnvField::Date,
                EnvField::System,
                EnvField::Shell,
                EnvField::Workspace
            ]
        );
    }

    #[test]
    fn parses_empty_env_to_no_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "quiet",
            "---\ndescription: quiet\nmode: stateless\nenv: []\n---\nYou are quiet.",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert!(files[0].env.is_empty());
    }

    #[test]
    fn parses_model_override() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "deep",
            "---\ndescription: reasons hard\nmode: stateless\nmodel: \"deepseek:deepseek-reasoner\"\nreasoning_effort: high\n---\nYou reason.",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert_eq!(files[0].model(), Some("deepseek:deepseek-reasoner"));
        assert_eq!(files[0].reasoning_effort(), Some(ReasoningEffort::High));
    }

    #[test]
    fn model_override_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "plain",
            "---\ndescription: x\nmode: stateless\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        assert_eq!(files[0].model(), None);
        assert_eq!(files[0].reasoning_effort(), None);
    }

    #[test]
    fn reserved_dir_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "coda",
            "---\ndescription: x\nmode: stateful\n---\nbody",
        );
        assert!(matches!(
            load_agent_files(dir.path()),
            Err(LoadError::ReservedName(_))
        ));
    }

    #[test]
    fn empty_body_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "memo",
            "---\ndescription: x\nmode: stateful\n---\n",
        );
        assert!(matches!(
            load_agent_files(dir.path()),
            Err(LoadError::Parse { .. })
        ));
    }

    #[test]
    fn referencing_reserved_subagent_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "boss",
            "---\ndescription: x\nmode: stateful\nsubagents: [coda]\n---\nbody",
        );
        assert!(matches!(
            load_agent_files(dir.path()),
            Err(LoadError::ReservedName(_))
        ));
    }

    #[test]
    fn unknown_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "explore",
            "---\ndescription: x\nmode: stateless\ntools: [no_such_tool]\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        let registry = ToolRegistry::new();
        let result = build_team(&registry, files, None, None);
        assert!(matches!(result, Err(LoadError::UnknownTool { .. })));
    }

    #[test]
    fn roots_exclude_referenced_agents() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "boss",
            "---\ndescription: x\nmode: stateful\nsubagents: [worker]\n---\nbody",
        );
        write_agent(
            dir.path(),
            "worker",
            "---\ndescription: x\nmode: stateless\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        let registry = ToolRegistry::new();
        let team = build_team(&registry, files, None, None).unwrap();

        // Only `boss` is a direct sub-agent of coda; `worker` hangs under boss.
        assert_eq!(team.root().subagents, vec!["boss".to_string()]);
    }

    #[test]
    fn self_referencing_agent_attaches_to_coda() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "loop",
            "---\ndescription: x\nmode: stateful\nsubagents: [loop]\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
        // A self-loop doesn't count as "referenced by another agent", so `loop`
        // is still a root under coda (and thus reachable), not orphaned.
        assert_eq!(team.root().subagents, vec!["loop".to_string()]);
    }

    #[test]
    fn cyclic_agents_are_dropped_as_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "---\ndescription: x\nmode: stateful\nsubagents: [b]\n---\nbody",
        );
        write_agent(
            dir.path(),
            "b",
            "---\ndescription: x\nmode: stateful\nsubagents: [a]\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        // `a` and `b` reference each other but neither is reachable from the
        // root, so both are dropped (with a warning) and the team still builds.
        let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
        assert!(team.root().subagents.is_empty());
    }

    #[test]
    fn tool_named_like_subagent_conflicts() {
        // A root-level agent named `grep` collides with coda's built-in `grep`
        // tool, since coda exposes both in one namespace.
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "grep",
            "---\ndescription: x\nmode: stateless\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        // Team construction catches the conflict from spec metadata alone — no
        // build.
        let result = build_team(&ToolRegistry::new(), files, None, None);
        assert!(matches!(
            result,
            Err(LoadError::Build(BuildError::NameConflict { .. }))
        ));
    }

    #[test]
    fn shared_subagent_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "a",
            "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
        );
        write_agent(
            dir.path(),
            "b",
            "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
        );
        write_agent(
            dir.path(),
            "shared",
            "---\ndescription: x\nmode: stateless\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
        let agents = team.build(".");
        assert!(agents.contains_key("shared"));
        assert!(agents.contains_key("a"));
        assert!(agents.contains_key("b"));
    }

    #[test]
    fn no_root_agent_file_is_all_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let root = load_root_agent_file(dir.path()).unwrap();
        assert!(root.tools.is_none());
        assert!(root.subagents.is_none());
        assert!(root.system_prompt.is_none());
    }

    #[test]
    fn root_agent_file_parses_overrides_and_body() {
        let dir = tempfile::tempdir().unwrap();
        write_root_agent(
            dir.path(),
            "---\ntools: [shell, read_file]\nsubagents: [explore]\n---\nYou are coda.",
        );
        let root = load_root_agent_file(dir.path()).unwrap();
        assert_eq!(
            root.tools,
            Some(vec!["shell".to_string(), "read_file".to_string()])
        );
        assert_eq!(root.subagents, Some(vec!["explore".to_string()]));
        assert_eq!(root.system_prompt.as_deref(), Some("You are coda."));
    }

    #[test]
    fn root_agent_file_empty_body_is_none() {
        let dir = tempfile::tempdir().unwrap();
        write_root_agent(dir.path(), "---\ntools: []\n---\n");
        let root = load_root_agent_file(dir.path()).unwrap();
        // Explicit empty list overrides to "no tools"; empty body falls back.
        assert_eq!(root.tools, Some(vec![]));
        assert!(root.system_prompt.is_none());
    }

    #[test]
    fn root_subagents_override_enables_root_sharing() {
        // `shared` is referenced by `boss`, so the default heuristic would hide
        // it from coda. An explicit root `subagents` list mounts it under coda
        // too — the same agent shared by root and another agent.
        let dir = tempfile::tempdir().unwrap();
        write_agent(
            dir.path(),
            "boss",
            "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
        );
        write_agent(
            dir.path(),
            "shared",
            "---\ndescription: x\nmode: stateless\n---\nbody",
        );
        let files = load_agent_files(dir.path()).unwrap();
        let team = build_team(
            &ToolRegistry::new(),
            files,
            None,
            Some(vec!["boss".into(), "shared".into()]),
        )
        .unwrap();
        assert_eq!(
            team.root().subagents,
            vec!["boss".to_string(), "shared".to_string()]
        );
    }

    #[test]
    fn root_tools_override_unknown_tool_errors() {
        let dir = tempfile::tempdir().unwrap();
        let files = load_agent_files(dir.path()).unwrap();
        let result = build_team(
            &ToolRegistry::new(),
            files,
            Some(vec!["no_such_tool".into()]),
            None,
        );
        assert!(matches!(
            result,
            Err(LoadError::UnknownTool { agent, .. }) if agent == ROOT_AGENT_NAME
        ));
    }

    #[test]
    fn tool_pattern_expands_to_matching_prebuilt_tools() {
        let registry = registry_with_mcp();
        let tools = resolve_tools(
            &registry,
            "explore",
            &["read_file".to_string(), "mcp__example__*".to_string()],
        )
        .unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        // Literal first, then the two example tools (sorted); other excluded.
        assert_eq!(
            names,
            vec!["read_file", "mcp__example__extract", "mcp__example__search"]
        );
    }

    #[test]
    fn tool_pattern_dedups_against_literal_overlap() {
        let registry = registry_with_mcp();
        // `mcp__example__search` named both explicitly and via the pattern must
        // appear once, or the downstream namespace check would reject it.
        let tools = resolve_tools(
            &registry,
            "explore",
            &[
                "mcp__example__search".to_string(),
                "mcp__example__*".to_string(),
            ],
        )
        .unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__example__search", "mcp__example__extract"]);
    }

    #[test]
    fn tool_pattern_matching_nothing_is_not_an_error() {
        let registry = registry_with_mcp();
        let tools = resolve_tools(&registry, "explore", &["mcp__nope__*".to_string()]).unwrap();
        assert!(tools.is_empty());
    }

    #[test]
    fn bare_star_is_not_a_wildcard() {
        // A bare `*` is not a pattern (omit `tools` to get everything); it has
        // no non-empty prefix, so it resolves like a literal and is unknown.
        let registry = registry_with_mcp();
        let result = resolve_tools(&registry, "explore", &["*".to_string()]);
        assert!(matches!(
            result,
            Err(LoadError::UnknownTool { tool, .. }) if tool == "*"
        ));
    }

    #[test]
    fn root_subagents_referencing_reserved_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_root_agent(dir.path(), "---\nsubagents: [coda]\n---\nbody");
        assert!(matches!(
            load_root_agent_file(dir.path()),
            Err(LoadError::ReservedName(_))
        ));
    }
}
