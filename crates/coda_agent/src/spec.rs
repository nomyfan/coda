use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use coda_tools::{BuildContext, TodoItem, ToolSpec};

use crate::agent::{Agent, AgentState, SubAgentMode, SubAgentTool};

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

        let todo_store = Arc::new(Mutex::new(Vec::<TodoItem>::new()));
        let state = Arc::new(Mutex::new(AgentState { messages: vec![] }));

        let tool_ctx = BuildContext {
            workspace_dir: ctx.workspace_dir.clone(),
            todo_store: Some(todo_store.clone()),
        };

        let mut agent = Agent {
            name: self.name.clone(),
            mode: self.mode.clone(),
            system_prompt: self.system_prompt.clone(),
            state,
            todo_store,
            tools: Default::default(),
            subagents: Default::default(),
        };

        for tool_spec in &self.tools {
            agent.tools.register(tool_spec.build(&tool_ctx));
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
