pub(crate) mod tools;

use crate::core::llm::LLMProvider;
use crate::core::session::Session;
use crate::core::tool::ToolManager;

pub(crate) struct Agent<P: LLMProvider> {
    name: String,
    pub(crate) provider: P,
    pub(crate) session: Session,
    pub(crate) tools: ToolManager,
}

impl<P: LLMProvider> Agent<P> {
    pub(crate) fn new(name: String, provider: P) -> Self {
        Agent {
            name,
            provider,
            session: Session::new(),
            tools: ToolManager::new(),
        }
    }
}
