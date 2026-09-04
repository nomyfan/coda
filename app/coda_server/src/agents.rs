//! File-based agent definitions.
//!
//! Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`:
//! YAML frontmatter (description, mode, tools, subagents, env, workspace, model,
//! reasoning_effort) followed by a markdown body used as the agent's system
//! prompt. They become
//! sub-agents of the top-level `coda` agent and may reference one another by name
//! to form deeper graphs.
//!
//! The top-level `coda` agent itself is configured by an optional
//! `.coda/agents/AGENT.md` (a bare file, distinct from the per-agent directories).
//! Its `tools`, `subagents`, and body each *explicitly override* a default when
//! present; otherwise the built-ins apply (all tools, auto-attached unreferenced
//! agents, and the built-in base prompt respectively). `coda` is always present.
//!
//! A [`ToolRegistry`] resolves `tools` includes and excludes: built-in tools by
//! name, plus any prebuilt tools (MCP, `ask_user`) registered at startup. The
//! original list form remains an include shorthand. A name ending in `*` over
//! a non-empty prefix is a pattern (e.g. `mcp__example__*`); a bare `*` is *not*
//! a wildcard. When include is absent, the root defaults to all tools and a
//! sub-agent defaults to none. Exclude always wins. An unknown plain name is a
//! hard error, surfaced at startup; a pattern that matches nothing only warns.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use coda_agent::{
    AgentSpec, AgentTeam, BuildError, SharedSystemPrompt, SubAgentMode, SystemPrompt,
};

use crate::{WorkspaceKnowledge, make_vars_provider};
use coda_core::tool::ToolObject;
use coda_tools::{BUILTIN_TOOL_NAMES, PrebuiltToolSpec, ToolSpec, spec_by_name};
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

/// Errors raised while assembling the global declarable-tool namespace.
#[derive(Debug, PartialEq, Eq)]
pub enum ToolRegistryError {
    /// A prebuilt tool has the same name as a builtin or an earlier prebuilt.
    DuplicateToolName(String),
}

impl std::fmt::Display for ToolRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolRegistryError::DuplicateToolName(name) => write!(
                f,
                "duplicate tool name '{name}': tool names must be globally unique"
            ),
        }
    }
}

impl std::error::Error for ToolRegistryError {}

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

    /// Register a prebuilt tool under its own `name()`.
    ///
    /// Tool names are global: colliding with a builtin or an earlier prebuilt
    /// is rejected instead of silently choosing one implementation.
    pub fn insert(&mut self, tool: Box<dyn ToolObject>) -> Result<(), ToolRegistryError> {
        let name = tool.name().to_string();
        if BUILTIN_TOOL_NAMES.contains(&name.as_str()) || self.prebuilt.contains_key(&name) {
            return Err(ToolRegistryError::DuplicateToolName(name));
        }
        self.prebuilt.insert(name, PrebuiltToolSpec::new(tool));
        Ok(())
    }

    fn contains(&self, name: &str) -> bool {
        BUILTIN_TOOL_NAMES.contains(&name) || self.prebuilt.contains_key(name)
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

    /// Every declarable tool name in the existing default order: builtins first,
    /// then prebuilt tools sorted by name.
    fn all_names(&self) -> Vec<String> {
        BUILTIN_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .chain(self.prebuilt.keys().cloned())
            .collect()
    }

    /// Expand a trailing-`*` prefix pattern to matching tool names, sorted by
    /// name. `prefix` is always non-empty (the caller rejects a bare `*`).
    fn expand_pattern(&self, prefix: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .all_names()
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .collect();
        names.sort_unstable();
        names
    }
}

/// An agent's `tools` selection. The list form is retained as include shorthand.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum ToolSelection {
    Include(Vec<String>),
    Rules(ToolRules),
}

/// Explicit include/exclude rules. A missing include uses the agent's existing
/// default, while an explicit empty include selects no declarable tools.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolRules {
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
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
    tools: Option<ToolSelection>,
    #[serde(default)]
    subagents: Vec<String>,
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
    reasoning_effort: Option<String>,
}

/// A parsed agent file (before tool resolution).
pub struct AgentFile {
    name: String,
    description: String,
    mode: SubAgentMode,
    tools: Option<ToolSelection>,
    subagents: Vec<String>,
    system_prompt: String,
    workspace: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
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
    pub fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.clone()
    }
}

/// Frontmatter of the optional top-level `.coda/agents/AGENT.md`. Both fields are
/// `Option` so "absent" (use the default) is distinct from an explicit empty
/// list (override to nothing).
#[derive(Deserialize, Default)]
struct RootFrontmatter {
    #[serde(default)]
    tools: Option<ToolSelection>,
    #[serde(default)]
    subagents: Option<Vec<String>>,
}

/// Parsed top-level `coda` configuration. Each field is an explicit override of a
/// default when `Some`/non-empty; otherwise the built-in behavior applies. See
/// [`build_agent_team`] (tools, sub-agents) and the system-prompt assembly (body).
#[derive(Default)]
pub struct RootAgentFile {
    pub tools: Option<ToolSelection>,
    pub subagents: Option<Vec<String>>,
    /// The body, used as the root agent's base system prompt; `None` when the
    /// file is absent or its body is empty (fall back to the built-in default).
    pub system_prompt: Option<String>,
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

/// Resolve tool names and patterns against `registry`, preserving first-seen
/// order and deduplicating overlaps.
///
/// A name ending in `*` with a non-empty prefix is a pattern (e.g.
/// `mcp__example__*`), expanded to every matching tool; a pattern matching
/// nothing only warns. A bare `*` and any unknown plain name are hard errors.
fn expand_tool_names(
    registry: &ToolRegistry,
    agent: &str,
    names: &[String],
) -> Result<Vec<String>, LoadError> {
    let mut expanded = Vec::with_capacity(names.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |expanded: &mut Vec<String>, name: String| {
        if seen.insert(name.clone()) {
            expanded.push(name);
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
            for matched in matches {
                push(&mut expanded, matched);
            }
        } else if registry.contains(name) {
            push(&mut expanded, name.clone());
        } else {
            return Err(LoadError::UnknownTool {
                agent: agent.to_string(),
                tool: name.clone(),
            });
        }
    }
    Ok(expanded)
}

#[derive(Clone, Copy)]
enum DefaultToolSet {
    All,
    Empty,
}

/// Resolve an agent's final declarable tools as `base - exclude`.
fn resolve_tools(
    registry: &ToolRegistry,
    agent: &str,
    selection: Option<&ToolSelection>,
    default: DefaultToolSet,
) -> Result<Vec<Box<dyn ToolSpec>>, LoadError> {
    let (include, exclude): (Option<&[String]>, &[String]) = match selection {
        None => (None, &[]),
        Some(ToolSelection::Include(include)) => (Some(include), &[]),
        Some(ToolSelection::Rules(rules)) => (rules.include.as_deref(), &rules.exclude),
    };

    let mut names = match include {
        Some(include) => expand_tool_names(registry, agent, include)?,
        None => match default {
            DefaultToolSet::All => registry.all_names(),
            DefaultToolSet::Empty => Vec::new(),
        },
    };
    let excluded: HashSet<String> = expand_tool_names(registry, agent, exclude)?
        .into_iter()
        .collect();
    names.retain(|name| !excluded.contains(name));

    Ok(names
        .iter()
        .map(|name| {
            registry
                .resolve(name)
                .expect("validated registry name resolves")
        })
        .collect())
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
/// Every agent's system prompt — root and sub alike — is the agent's base body
/// (its own template) plus a per-turn variable provider rooted at that agent's
/// workspace. The provider's bindings (`{{date}}`, `{{workspace}}`,
/// `{{workspace_available_skills}}`, `{{workspace_custom_instructions}}`, …) are
/// substituted into the base body; the workspace's skills and `AGENTS.md` are
/// two of those bindings, sourced from that workspace's hot-reloaded
/// [`WorkspaceKnowledge`] handles (looked up in `knowledge`). Each agent's tool
/// root is recorded on the returned team via [`AgentTeam::with_agent_workspaces`].
/// `agent_workspaces` maps sub-agent names to their resolved workspace; an agent
/// absent there (and the root) uses `root_workspace`.
#[allow(clippy::too_many_arguments)]
pub fn build_agent_team(
    root_workspace: &str,
    root_base: SharedSystemPrompt,
    knowledge: &HashMap<String, WorkspaceKnowledge>,
    agent_workspaces: &HashMap<String, String>,
    registry: &ToolRegistry,
    files: Vec<AgentFile>,
    root_tools: Option<ToolSelection>,
    root_subagents: Option<Vec<String>>,
) -> Result<AgentTeam, LoadError> {
    // Assemble a prompt for an agent rooted at `workspace`: its base body plus a
    // per-turn variable provider carrying that workspace's knowledge handles.
    // Every dynamic piece (env, skills, AGENTS.md) is a `{{name}}` binding the
    // base body composes.
    let assemble = |base: SharedSystemPrompt, workspace: &str| {
        let knowledge = knowledge
            .get(workspace)
            .cloned()
            .unwrap_or_else(WorkspaceKnowledge::empty);
        SystemPrompt::new(base).with_vars(make_vars_provider(workspace.to_string(), knowledge))
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

    let root_tools = resolve_tools(
        registry,
        ROOT_AGENT_NAME,
        root_tools.as_ref(),
        DefaultToolSet::All,
    )?;

    let root = AgentSpec {
        name: ROOT_AGENT_NAME.to_string(),
        description: String::new(),
        system_prompt: assemble(root_base, root_workspace),
        mode: SubAgentMode::Stateful,
        tools: root_tools,
        subagents: roots,
    };

    let mut subagents = Vec::with_capacity(files.len());
    for file in files {
        let tools = resolve_tools(
            registry,
            &file.name,
            file.tools.as_ref(),
            DefaultToolSet::Empty,
        )?;
        let workspace = agent_workspaces
            .get(&file.name)
            .map(String::as_str)
            .unwrap_or(root_workspace);
        let system_prompt = assemble(SharedSystemPrompt::new(file.system_prompt), workspace);
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
    let raw = match raw.map(str::trim) {
        // Trim incidental YAML whitespace so `workspace: "./sub "` resolves like
        // `./sub` instead of failing with a confusing "missing dir".
        Some(raw) if !raw.is_empty() => raw,
        _ => return Ok(root_workspace.to_string()),
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
#[path = "agents_tests.rs"]
mod tests;
