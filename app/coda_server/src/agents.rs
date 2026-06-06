//! File-based agent definitions.
//!
//! Sub-agents are declared one-per-directory under `.coda/agents/<name>/AGENT.md`:
//! YAML frontmatter (description, mode, tools, subagents) followed by a markdown
//! body used as the agent's system prompt. They become sub-agents of the
//! top-level `coda` agent and may reference one another by name to form deeper
//! graphs.
//!
//! The top-level `coda` agent itself is configured by an optional
//! `.coda/agents/AGENT.md` (a bare file, distinct from the per-agent directories).
//! Its `tools`, `subagents`, and body each *explicitly override* a default when
//! present; otherwise the built-ins apply (all tools, auto-attached unreferenced
//! agents, and the built-in base prompt respectively). `coda` is always present.
//!
//! A [`ToolRegistry`] resolves the `tools` list: built-in tools by name, plus
//! any prebuilt tools (MCP, `ask_user`) registered at startup. An unknown tool
//! name is a hard error, surfaced at startup.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use coda_agent::{AgentSpec, AgentTeam, BuildError, SubAgentMode, SystemPrompt};
use coda_core::tool::ToolObject;
use coda_tools::{PrebuiltToolSpec, ToolSpec, builtin_specs, spec_by_name};
use serde::Deserialize;
use tracing::info;

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
    /// The assembled team failed structural validation: duplicate names,
    /// dangling sub-agent references, tool/sub-agent namespace conflicts, or an
    /// agent left unreachable from the top-level `coda` agent (e.g. a reference
    /// cycle with no entry point).
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
}

/// A parsed agent file (before tool resolution).
pub struct AgentFile {
    name: String,
    description: String,
    mode: SubAgentMode,
    tools: Vec<String>,
    subagents: Vec<String>,
    system_prompt: String,
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
}

/// Parsed top-level `coda` configuration. Each field is an explicit override of a
/// default when `Some`/non-empty; otherwise the built-in behavior applies. See
/// [`build_agent_team`] (tools, sub-agents) and the system-prompt assembly (body).
#[derive(Default)]
pub struct RootAgentFile {
    pub tools: Option<Vec<String>>,
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

/// Resolve a list of tool names to [`ToolSpec`] factories via `registry`, failing
/// with [`LoadError::UnknownTool`] (attributed to `agent`) on the first miss.
fn resolve_tools(
    registry: &ToolRegistry,
    agent: &str,
    names: &[String],
) -> Result<Vec<Box<dyn ToolSpec>>, LoadError> {
    let mut tools = Vec::with_capacity(names.len());
    for name in names {
        match registry.resolve(name) {
            Some(spec) => tools.push(spec),
            None => {
                return Err(LoadError::UnknownTool {
                    agent: agent.to_string(),
                    tool: name.clone(),
                });
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
/// references, tool/sub-agent conflicts, or agents unreachable from `coda`
/// ([`LoadError::Build`]).
pub fn build_agent_team(
    root_system_prompt: SystemPrompt,
    registry: &ToolRegistry,
    files: Vec<AgentFile>,
    root_tools: Option<Vec<String>>,
    root_subagents: Option<Vec<String>>,
) -> Result<AgentTeam, LoadError> {
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
        system_prompt: root_system_prompt,
        mode: SubAgentMode::Stateful,
        tools: root_tools,
        subagents: roots,
    };

    let mut subagents = Vec::with_capacity(files.len());
    for file in files {
        let tools = resolve_tools(registry, &file.name, &file.tools)?;
        subagents.push(AgentSpec {
            name: file.name,
            description: file.description,
            system_prompt: SystemPrompt::Static(file.system_prompt),
            mode: file.mode,
            tools,
            subagents: file.subagents,
        });
    }

    AgentTeam::new(root, subagents).map_err(LoadError::Build)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn no_config_dir_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_agent_files(dir.path()).unwrap().is_empty());
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
        let result = build_agent_team("root".into(), &registry, files, None, None);
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
        let team = build_agent_team("root".into(), &registry, files, None, None).unwrap();

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
        let team =
            build_agent_team("root".into(), &ToolRegistry::new(), files, None, None).unwrap();
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
        let team =
            build_agent_team("root".into(), &ToolRegistry::new(), files, None, None).unwrap();
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
        let result = build_agent_team("root".into(), &ToolRegistry::new(), files, None, None);
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
        let team =
            build_agent_team("root".into(), &ToolRegistry::new(), files, None, None).unwrap();
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
        let team = build_agent_team(
            "root".into(),
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
        let result = build_agent_team(
            "root".into(),
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
    fn root_subagents_referencing_reserved_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_root_agent(dir.path(), "---\nsubagents: [coda]\n---\nbody");
        assert!(matches!(
            load_root_agent_file(dir.path()),
            Err(LoadError::ReservedName(_))
        ));
    }
}
