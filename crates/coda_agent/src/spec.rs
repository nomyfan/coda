use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::Mutex;

use coda_tools::{BuildContext, TodoItem, ToolSpec};

use crate::agent::{Agent, AgentState, SubAgentMode, SubAgentTool, SystemPrompt};

/// Errors that can occur while validating an [`AgentTeam`].
#[derive(Debug)]
pub enum BuildError {
    /// Two specs share the same name.
    DuplicateAgentName(String),
    /// A spec references a sub-agent by a name that has no corresponding spec.
    UnknownSubagent { parent: String, child: String },
    /// Within one agent, a tool and a sub-agent (or two tools/sub-agents) share
    /// a name. Tools and sub-agents occupy the same LLM tool namespace, so the
    /// name would be ambiguous at dispatch.
    NameConflict { agent: String, name: String },
    /// One or more sub-agents cannot be reached from the root (e.g. a reference
    /// cycle with no entry point), so they could never be invoked.
    UnreachableAgents(Vec<String>),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::DuplicateAgentName(name) => {
                write!(
                    f,
                    "Duplicate agent name '{}': agent names must be globally unique",
                    name
                )
            }
            BuildError::UnknownSubagent { parent, child } => {
                write!(
                    f,
                    "Agent '{}' references unknown sub-agent '{}'",
                    parent, child
                )
            }
            BuildError::NameConflict { agent, name } => {
                write!(
                    f,
                    "Agent '{}' has a tool and sub-agent both named '{}' (tools and \
                     sub-agents share one namespace)",
                    agent, name
                )
            }
            BuildError::UnreachableAgents(names) => {
                write!(
                    f,
                    "sub-agents unreachable from the root (no entry point — check for \
                     reference cycles): {}",
                    names.join(", ")
                )
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// Declarative specification for a single agent — plain data. Sub-agents are
/// referenced by name, resolved against the sibling specs held by an
/// [`AgentTeam`]. The same agent may be a sub-agent of several parents.
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub system_prompt: SystemPrompt,
    pub mode: SubAgentMode,
    pub tools: Vec<Box<dyn ToolSpec>>,
    /// Names of this agent's sub-agents (must resolve to sibling specs).
    pub subagents: Vec<String>,
}

/// A validated, rooted team of agents: one [`root`](AgentTeam::root) (the entry
/// point) plus its sub-agents. Holding one is proof that the team is sound —
/// agent names are unique, every sub-agent reference resolves, within each agent
/// no name is shared by a tool and a sub-agent (they occupy one LLM namespace),
/// and every sub-agent is reachable from the root. Validation happens once, in
/// [`AgentTeam::new`]; consequently [`AgentTeam::build`] cannot fail.
///
/// It is the per-session factory: each [`build`](AgentTeam::build) mints a fresh
/// set of [`Agent`]s with independent state, so one team backs many sessions.
pub struct AgentTeam {
    root: AgentSpec,
    subagents: Vec<AgentSpec>,
}

impl AgentTeam {
    /// The sole constructor: validates `root` together with `subagents` from
    /// [`ToolSpec`] metadata alone (no tools are built, so no `build` cost or
    /// side effects), returning a validated team or the first [`BuildError`].
    /// There is deliberately no way to obtain an `AgentTeam` without passing this
    /// gate.
    pub fn new(root: AgentSpec, subagents: Vec<AgentSpec>) -> Result<Self, BuildError> {
        // Index every spec by name, rejecting duplicates across root + subagents.
        let mut by_name: HashMap<&str, &AgentSpec> = HashMap::new();
        for spec in std::iter::once(&root).chain(&subagents) {
            if by_name.insert(spec.name.as_str(), spec).is_some() {
                return Err(BuildError::DuplicateAgentName(spec.name.clone()));
            }
        }

        for spec in std::iter::once(&root).chain(&subagents) {
            for child in &spec.subagents {
                if !by_name.contains_key(child.as_str()) {
                    return Err(BuildError::UnknownSubagent {
                        parent: spec.name.clone(),
                        child: child.clone(),
                    });
                }
            }
            // Tools and sub-agents share one LLM namespace.
            let mut names: HashSet<&str> = HashSet::new();
            for tool in &spec.tools {
                if !names.insert(tool.name()) {
                    return Err(BuildError::NameConflict {
                        agent: spec.name.clone(),
                        name: tool.name().to_string(),
                    });
                }
            }
            for child in &spec.subagents {
                if !names.insert(child.as_str()) {
                    return Err(BuildError::NameConflict {
                        agent: spec.name.clone(),
                        name: child.clone(),
                    });
                }
            }
        }

        // Every sub-agent must be reachable from the root; otherwise it could
        // never be invoked. A visited set keeps cycles from looping forever.
        let mut reachable: HashSet<&str> = HashSet::new();
        let mut stack = vec![root.name.as_str()];
        while let Some(name) = stack.pop() {
            if reachable.insert(name)
                && let Some(spec) = by_name.get(name)
            {
                stack.extend(spec.subagents.iter().map(String::as_str));
            }
        }
        let unreachable: Vec<String> = subagents
            .iter()
            .filter(|s| !reachable.contains(s.name.as_str()))
            .map(|s| s.name.clone())
            .collect();
        if !unreachable.is_empty() {
            return Err(BuildError::UnreachableAgents(unreachable));
        }

        Ok(AgentTeam { root, subagents })
    }

    /// The root agent's spec — the team's entry point.
    pub fn root(&self) -> &AgentSpec {
        &self.root
    }

    /// The root agent's name.
    pub fn root_name(&self) -> &str {
        &self.root.name
    }

    /// The non-root agent specs.
    pub fn subagents(&self) -> &[AgentSpec] {
        &self.subagents
    }

    /// Build every spec into a fresh [`Agent`], keyed by name, with tools rooted
    /// at `workspace_dir`. Each spec is built exactly once, so the same agent may
    /// be a sub-agent of several parents. Cycles are fine: sub-agents are
    /// addressed by name through the resulting flat map, so building never
    /// recurses. Infallible — the team was validated at construction. Call once
    /// per session: the returned agents carry independent state.
    pub fn build(&self, workspace_dir: &str) -> HashMap<String, Agent> {
        let all = || std::iter::once(&self.root).chain(&self.subagents);
        let by_name: HashMap<&str, &AgentSpec> = all().map(|s| (s.name.as_str(), s)).collect();

        let mut agents = HashMap::new();
        for spec in all() {
            let todo_store = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
            let state = Arc::new(Mutex::new(AgentState { messages: vec![] }));

            let tool_ctx = BuildContext {
                workspace_dir: workspace_dir.to_string(),
                todo_store: todo_store.clone(),
            };

            let mut agent = Agent {
                name: spec.name.clone(),
                mode: spec.mode.clone(),
                system_prompt: spec.system_prompt.clone(),
                state,
                todo_store,
                tools: Default::default(),
                subagents: Default::default(),
            };

            for tool_spec in &spec.tools {
                agent.tools.register(tool_spec.build(&tool_ctx));
            }
            for child in &spec.subagents {
                // Validated at construction, so the reference always resolves.
                let sub = by_name[child.as_str()];
                agent.subagents.register(SubAgentTool {
                    name: sub.name.clone(),
                    description: sub.description.clone(),
                    mode: sub.mode.clone(),
                });
            }
            agents.insert(spec.name.clone(), agent);
        }

        agents
    }
}
