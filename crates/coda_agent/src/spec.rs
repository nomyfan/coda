use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{Capabilities, Capability, Program};

use coda_execution::BackgroundTasks;
use coda_tools::{BuildContext, KeyedLock, SYNTHETIC_RESERVED_TOOL_NAMES, ToolSpec};

use crate::agent::{SUBAGENT_TOOL_PREFIX, SubAgentMode, SubAgentTool, SystemPrompt};

/// OpenAI-compatible function names are capped at 64 characters; a sub-agent's
/// name plus the `agent__` prefix must fit, or the provider rejects every
/// request that exposes it.
const MAX_TOOL_NAME_LEN: usize = 64;

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
    /// A configured tool claims a name reserved for a provider-visible
    /// synthetic tool owned by the runtime.
    ReservedToolName {
        /// Agent whose configured tools contain the reserved name.
        agent: String,
        /// Provider-visible synthetic name claimed by the configured tool.
        name: String,
    },
    /// A sub-agent name would shadow a runtime-owned tool through bare-name dispatch.
    ReservedSubagentName { name: String },
    /// A sub-agent's name, once prefixed for the LLM tool namespace, exceeds the
    /// provider's function-name length limit.
    SubagentNameTooLong { name: String, max: usize },
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
                    "Agent '{}' has two entries named '{}' (tools and sub-agents \
                     share one namespace)",
                    agent, name
                )
            }
            BuildError::ReservedToolName { agent, name } => {
                write!(
                    f,
                    "Agent '{}' configures tool '{}', but that name is reserved by the runtime",
                    agent, name
                )
            }
            BuildError::ReservedSubagentName { name } => {
                write!(
                    f,
                    "Sub-agent name '{name}' is reserved by the runtime; rename the sub-agent"
                )
            }
            BuildError::SubagentNameTooLong { name, max } => {
                write!(
                    f,
                    "Sub-agent name '{}' is too long: prefixed with '{}' the tool \
                     name must be at most {} characters",
                    name, SUBAGENT_TOOL_PREFIX, max
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
    pub capabilities: Capabilities,
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
/// set of session-bound [`Program`]s, so one team backs many sessions.
pub struct AgentTeam {
    root: AgentSpec,
    subagents: Vec<AgentSpec>,
    /// Per-agent tool-root override, keyed by agent name. An agent absent here
    /// roots its tools at the default workspace passed to [`build`](Self::build).
    /// This is the lookup a per-agent workspace flows through — the root is just
    /// another entry, not a special case.
    agent_workspaces: HashMap<String, String>,
}

impl AgentTeam {
    /// The sole constructor: validates `root` together with `subagents` from
    /// [`ToolSpec`] metadata alone (no tools are built, so no `build` cost or
    /// side effects), returning a validated team or the first [`BuildError`].
    /// There is deliberately no way to obtain an `AgentTeam` without passing this
    /// gate.
    pub fn new(root: AgentSpec, mut subagents: Vec<AgentSpec>) -> Result<Self, BuildError> {
        for subagent in &subagents {
            if SYNTHETIC_RESERVED_TOOL_NAMES.contains(&subagent.name.as_str()) {
                return Err(BuildError::ReservedSubagentName {
                    name: subagent.name.clone(),
                });
            }
        }
        // Index every spec by name, rejecting duplicates across root + subagents.
        let mut by_name: HashMap<&str, &AgentSpec> = HashMap::new();
        for spec in std::iter::once(&root).chain(&subagents) {
            if by_name.insert(spec.name.as_str(), spec).is_some() {
                return Err(BuildError::DuplicateAgentName(spec.name.clone()));
            }
        }

        for spec in std::iter::once(&root).chain(&subagents) {
            for child in &spec.subagents {
                if SYNTHETIC_RESERVED_TOOL_NAMES.contains(&child.as_str()) {
                    return Err(BuildError::ReservedSubagentName {
                        name: child.clone(),
                    });
                }
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
                if SYNTHETIC_RESERVED_TOOL_NAMES.contains(&tool.name()) {
                    return Err(BuildError::ReservedToolName {
                        agent: spec.name.clone(),
                        name: tool.name().to_string(),
                    });
                }
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

        // Walk the reference graph from the root. Sub-agents that can't be
        // reached (e.g. a reference cycle with no entry point) could never be
        // invoked, so they're dropped rather than failing the whole team.
        // A visited set keeps cycles from looping forever.
        let mut reachable: HashSet<String> = HashSet::new();
        let mut stack = vec![root.name.as_str()];
        while let Some(name) = stack.pop() {
            if reachable.insert(name.to_string())
                && let Some(spec) = by_name.get(name)
            {
                stack.extend(spec.subagents.iter().map(String::as_str));
            }
        }
        subagents.retain(|s| {
            let keep = reachable.contains(&s.name);
            if !keep {
                tracing::warn!(
                    agent = %s.name,
                    "ignoring sub-agent unreachable from the root (no entry point — check for reference cycles)"
                );
            }
            keep
        });

        // Retained sub-agents are exposed to the LLM as `agent__<name>` tools.
        for spec in &subagents {
            if SUBAGENT_TOOL_PREFIX.len() + spec.name.len() > MAX_TOOL_NAME_LEN {
                return Err(BuildError::SubagentNameTooLong {
                    name: spec.name.clone(),
                    max: MAX_TOOL_NAME_LEN,
                });
            }
        }

        Ok(AgentTeam {
            root,
            subagents,
            agent_workspaces: HashMap::new(),
        })
    }

    /// Override per-agent tool roots by name. Names not present fall back to the
    /// default workspace passed to [`build`](Self::build); names that don't match
    /// any agent are simply never looked up. Returns `self` for chaining off
    /// [`new`](Self::new).
    pub fn with_agent_workspaces(mut self, workspaces: HashMap<String, String>) -> Self {
        self.agent_workspaces = workspaces;
        self
    }

    /// The root agent's spec — the team's entry point.
    pub fn root(&self) -> &AgentSpec {
        &self.root
    }

    /// Build every spec into a session-bound [`Program`], keyed by name. Tools are rooted
    /// at the agent's own workspace ([`with_agent_workspaces`](Self::with_agent_workspaces)),
    /// falling back to `default_workspace` for any agent without an override.
    /// Each spec is built exactly once, so the same agent may be a sub-agent of
    /// several parents. Cycles are fine: sub-agents are addressed by name through
    /// the resulting flat map, so building never recurses. Infallible — the team
    /// was validated at construction. Call once per session: the returned programs
    /// bind session resources but allocate no process memory.
    ///
    /// `file_locks` is the deliberate exception to "session-bound resources": the
    /// same registry must reach every call in the process, or concurrent
    /// `edit_file`s from sibling agents or from two sessions over one workspace
    /// clobber each other. Agents with background capability receive the same
    /// session task registry; disabling it on one agent leaves others unaffected.
    pub fn build(
        &self,
        default_workspace: &str,
        file_locks: Arc<KeyedLock<String>>,
        background: Option<Arc<BackgroundTasks>>,
    ) -> HashMap<String, Arc<Program>> {
        let all = || std::iter::once(&self.root).chain(&self.subagents);
        let by_name: HashMap<&str, &AgentSpec> = all().map(|s| (s.name.as_str(), s)).collect();

        let mut agents = HashMap::new();
        for spec in all() {
            let workspace_dir = self
                .agent_workspaces
                .get(&spec.name)
                .map(String::as_str)
                .unwrap_or(default_workspace);
            let tool_ctx = BuildContext {
                workspace_dir: workspace_dir.to_string(),
                file_locks: file_locks.clone(),
                agent_name: spec.name.clone(),
                background: background
                    .clone()
                    .filter(|_| spec.capabilities.contains(Capability::Background)),
            };

            let mut agent = Program {
                capabilities: spec.capabilities.clone(),
                name: spec.name.clone(),
                mode: spec.mode.clone(),
                system_prompt: spec.system_prompt.clone(),
                tools: Default::default(),
                subagents: Default::default(),
            };

            for tool_spec in &spec.tools {
                agent.tools.register(tool_spec.build(&tool_ctx));
            }
            for tool_spec in coda_tools::background_specs(tool_ctx.background.as_ref()) {
                agent.tools.register(tool_spec.build(&tool_ctx));
            }
            if spec.capabilities.contains(Capability::Ptc) {
                agent
                    .tools
                    .register(coda_tools::RunJavaScriptToolSpec.build(&tool_ctx));
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
            agents.insert(spec.name.clone(), Arc::new(agent));
        }

        agents
    }
}

#[cfg(test)]
#[path = "spec_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "spec_capabilities_tests.rs"]
mod capability_tests;
