pub(crate) mod tools;

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::core::llm::{LLMProvider, Message};
use crate::core::tool::ToolManager;

#[derive(Debug, Clone)]
pub(crate) struct TodoItem {
    pub(crate) title: String,
    pub(crate) done: bool,
}

pub(crate) struct AgentState {
    messages: Vec<Message>,
    pub(crate) todos: Vec<TodoItem>,
}

pub(crate) struct Agent<P: LLMProvider> {
    pub(crate) provider: P,
    pub(crate) state: Arc<Mutex<AgentState>>,
    pub(crate) tools: ToolManager,
}

impl<P: LLMProvider> Agent<P> {
    pub(crate) fn new(provider: P) -> Self {
        let state = Arc::new(Mutex::new(AgentState {
            messages: vec![],
            todos: vec![],
        }));

        Agent {
            provider,
            state,
            tools: ToolManager::new(),
        }
    }

    pub(crate) fn state(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    pub(crate) async fn add_message(&self, message: Message) {
        self.state.lock().await.messages.push(message);
    }

    pub(crate) async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }
}
