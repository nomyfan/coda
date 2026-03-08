use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::tool::{ToolObject, ToolWrapper};

use crate::agent::{Agent, AgentState, SubAgentMode, SubAgentTool};
use crate::tools::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

/// Runtime context for building agents and tools.
pub struct BuildContext {
    pub workspace_dir: String,
}

/// A factory for creating tool instances.
pub trait ToolSpec: Send + Sync {
    fn build(&self, state: &Arc<Mutex<AgentState>>, ctx: &BuildContext) -> Box<dyn ToolObject>;
}

/// Specification for a sub-agent.
pub struct SubAgentSpec {
    pub name: String,
    pub description: String,
    pub mode: SubAgentMode,
    pub agent: AgentSpec,
}

/// Declarative specification for building an agent.
pub struct AgentSpec {
    pub name: String,
    pub system_prompt: String,
    pub tools: Vec<Box<dyn ToolSpec>>,
    pub subagents: Vec<SubAgentSpec>,
}

impl AgentSpec {
    pub fn build(&self, ctx: &BuildContext) -> Agent {
        let mut agent = Agent::new(&self.name, self.system_prompt.to_string());
        agent.system_prompt = self.system_prompt.clone();

        // Build and register tools
        for tool_spec in &self.tools {
            agent.tools.register(tool_spec.build(&agent.state, ctx));
        }

        // Recursively build and register subagents
        for sub_spec in &self.subagents {
            let sub_agent = sub_spec.agent.build(ctx);
            agent.subagents.register(SubAgentTool {
                name: sub_spec.name.clone(),
                description: sub_spec.description.clone(),
                agent: Mutex::new(sub_agent),
                mode: sub_spec.mode.clone(),
            });
        }

        agent
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
