use std::fmt::Display;
use std::sync::Arc;

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::agent::{AgentState, TodoItem};
use coda_core::tool::{Tool, ToolResult};

// --- ReadTodosTool ---

pub struct ReadTodosTool {
    state: Arc<Mutex<AgentState>>,
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReadTodosParams {}

pub struct ReadTodosOutput(Vec<TodoItem>);

impl Display for ReadTodosOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return write!(f, "No todos.");
        }
        for (i, item) in self.0.iter().enumerate() {
            let status = if item.done { "x" } else { " " };
            writeln!(f, "{}. [{}] {}", i + 1, status, item.title)?;
        }
        Ok(())
    }
}

impl ReadTodosTool {
    pub fn new(state: Arc<Mutex<AgentState>>) -> Self {
        let schema = schemars::schema_for!(ReadTodosParams);
        ReadTodosTool { state, schema }
    }
}

impl Tool for ReadTodosTool {
    type Parameters = ReadTodosParams;
    type Output = ReadTodosOutput;

    fn name(&self) -> &str {
        "read_todos"
    }

    fn description(&self) -> &str {
        "Read all todo items."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        _params: Self::Parameters,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let state = self.state.clone();

        async move {
            let s = state.lock().await;
            Ok(ReadTodosOutput(s.todos.clone()))
        }
    }
}

// --- WriteTodosTool ---

pub struct WriteTodosTool {
    state: Arc<Mutex<AgentState>>,
    schema: Schema,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteTodosItem {
    /// The title of the todo item.
    title: String,
    /// Whether the todo item is done.
    done: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WriteTodosParams {
    /// The complete list of todo items to replace the current list.
    todos: Vec<WriteTodosItem>,
}

impl WriteTodosTool {
    pub fn new(state: Arc<Mutex<AgentState>>) -> Self {
        let schema = schemars::schema_for!(WriteTodosParams);
        WriteTodosTool { state, schema }
    }
}

impl Tool for WriteTodosTool {
    type Parameters = WriteTodosParams;
    type Output = String;

    fn name(&self) -> &str {
        "write_todos"
    }

    fn description(&self) -> &str {
        "Replace the entire todo list. You should read the todos first, then write the updated list."
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.schema.as_value()
    }

    #[allow(clippy::manual_async_fn)]
    fn execute(
        &self,
        params: Self::Parameters,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let state = self.state.clone();

        async move {
            let mut s = state.lock().await;
            s.todos = params
                .todos
                .into_iter()
                .map(|item| TodoItem {
                    title: item.title,
                    done: item.done,
                })
                .collect();
            Ok(format!("Todos updated. {} items.", s.todos.len()))
        }
    }
}
