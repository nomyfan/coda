//! Session-bound executable definitions shared by independent processes.
use crate::agent::{SubAgentMode, SubAgents, SystemPrompt};
use coda_core::tool::Tools;

pub struct Program {
    pub capabilities: crate::Capabilities,
    pub name: String,
    pub mode: SubAgentMode,
    pub system_prompt: SystemPrompt,
    pub tools: Tools,
    pub subagents: SubAgents,
}
