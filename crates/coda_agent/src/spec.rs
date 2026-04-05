use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::tool::{ToolObject, ToolWrapper};

use crate::agent::{Agent, AgentState, SubAgentMode, SubAgentTool};
use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

/// Errors that can occur while building an agent tree from its spec.
#[derive(Debug)]
pub enum BuildError {
    /// Two agents in the spec tree share the same name.
    DuplicateAgentName(String),
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
        }
    }
}

impl std::error::Error for BuildError {}

/// Runtime context for building agents and tools.
pub struct BuildContext {
    pub workspace_dir: String,
}

/// A factory for creating tool instances.
pub trait ToolSpec: Send + Sync {
    fn build(&self, state: &Arc<Mutex<AgentState>>, ctx: &BuildContext) -> Box<dyn ToolObject>;
}

/// Declarative specification for building an agent.
pub struct AgentSpec {
    pub name: String,
    pub description: String,
    pub system_prompt: String,
    pub mode: SubAgentMode,
    pub tools: Vec<Box<dyn ToolSpec>>,
    pub subagents: Vec<AgentSpec>,
}

impl AgentSpec {
    pub fn build(&self, ctx: &BuildContext) -> Result<HashMap<String, Agent>, BuildError> {
        let mut agents = HashMap::new();
        self.build_agent(ctx, &mut agents)?;
        Ok(agents)
    }

    fn build_agent(
        &self,
        ctx: &BuildContext,
        agents: &mut HashMap<String, Agent>,
    ) -> Result<(), BuildError> {
        if agents.contains_key(&self.name) {
            return Err(BuildError::DuplicateAgentName(self.name.clone()));
        }

        let state = Arc::new(Mutex::new(AgentState {
            messages: vec![],
            todos: vec![],
        }));

        let mut agent = Agent {
            name: self.name.clone(),
            mode: self.mode.clone(),
            system_prompt: self.system_prompt.clone(),
            state,
            tools: Default::default(),
            subagents: Default::default(),
        };

        for tool_spec in &self.tools {
            agent.tools.register(tool_spec.build(&agent.state, ctx));
        }
        for sub_spec in &self.subagents {
            agent.subagents.register(SubAgentTool {
                name: sub_spec.name.clone(),
                description: sub_spec.description.clone(),
                mode: sub_spec.mode.clone(),
            });
        }
        agents.insert(self.name.clone(), agent);

        for sub_spec in &self.subagents {
            sub_spec.build_agent(ctx, agents)?;
        }
        Ok(())
    }
}

// --- Built-in tool specs ---

pub struct ShellToolSpec;

impl ToolSpec for ShellToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ShellTool::new()))
    }
}

pub struct ReadFileToolSpec;

impl ToolSpec for ReadFileToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadFileTool::new()))
    }
}

pub struct WriteFileToolSpec;

impl ToolSpec for WriteFileToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteFileTool::new()))
    }
}

pub struct ListDirectoryToolSpec;

impl ToolSpec for ListDirectoryToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ListDirectoryTool::new()))
    }
}

pub struct GrepToolSpec;

impl ToolSpec for GrepToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GrepTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct GlobToolSpec;

impl ToolSpec for GlobToolSpec {
    fn build(&self, _state: &Arc<Mutex<AgentState>>, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GlobTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct ReadTodosToolSpec;

impl ToolSpec for ReadTodosToolSpec {
    fn build(&self, state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadTodosTool::new(state.clone())))
    }
}

pub struct WriteTodosToolSpec;

impl ToolSpec for WriteTodosToolSpec {
    fn build(&self, state: &Arc<Mutex<AgentState>>, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteTodosTool::new(state.clone())))
    }
}

/// Returns builtin tool specs for a standard agent.
pub fn builtin_specs() -> Vec<Box<dyn ToolSpec>> {
    vec![
        Box::new(ShellToolSpec),
        Box::new(ReadFileToolSpec),
        Box::new(WriteFileToolSpec),
        Box::new(ListDirectoryToolSpec),
        Box::new(GrepToolSpec),
        Box::new(GlobToolSpec),
        Box::new(ReadTodosToolSpec),
        Box::new(WriteTodosToolSpec),
    ]
}
