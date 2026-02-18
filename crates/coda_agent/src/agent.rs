use std::sync::Arc;
use tokio::sync::Mutex;

use coda_core::llm::{LLMProvider, Message};
use coda_core::tool::ToolManager;

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

pub struct AgentState {
    messages: Vec<Message>,
    pub todos: Vec<TodoItem>,
}

pub struct Agent<P: LLMProvider> {
    pub provider: P,
    pub state: Arc<Mutex<AgentState>>,
    pub tools: ToolManager,
}

impl<P: LLMProvider> Agent<P> {
    pub fn new(provider: P) -> Self {
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

    pub fn state(&self) -> Arc<Mutex<AgentState>> {
        self.state.clone()
    }

    pub async fn add_message(&self, message: Message) {
        self.state.lock().await.messages.push(message);
    }

    pub async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.messages.clone()
    }
}
