use std::fmt::Display;

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};

use coda_core::tool::{Tool, ToolCallContext, ToolResult};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub title: String,
    pub done: bool,
}

/// The `kind` these tools record their list under. Opaque to the runtime, which
/// stores and cuts it without knowing what it holds.
const TODOS: &str = "todos";

// --- ReadTodosTool ---

pub struct ReadTodosTool {
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

impl Default for ReadTodosTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadTodosTool {
    pub fn new() -> Self {
        let schema = schemars::schema_for!(ReadTodosParams);
        ReadTodosTool { schema }
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let todos = ctx
            .state
            .get(TODOS)
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        async move { Ok(ReadTodosOutput(todos)) }
    }
}

// --- WriteTodosTool ---

/// Replaces the list. Keeps nothing of its own: the new list goes to
/// [`ToolCallContext::state`], which anchors it to the message recording this
/// call, so it is cut by a fork or a rewind along with the turn that wrote it.
pub struct WriteTodosTool {
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

impl WriteTodosParams {
    fn into_items(self) -> Vec<TodoItem> {
        self.todos
            .into_iter()
            .map(|item| TodoItem {
                title: item.title,
                done: item.done,
            })
            .collect()
    }
}

impl Default for WriteTodosTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteTodosTool {
    pub fn new() -> Self {
        let schema = schemars::schema_for!(WriteTodosParams);
        WriteTodosTool { schema }
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
        ctx: ToolCallContext,
    ) -> impl Future<Output = ToolResult<Self::Output>> + Send + 'static {
        let todos = params.into_items();
        let count = todos.len();
        // A complete list, never a delta — see `ThreadState`. Recorded only if
        // this call reaches here, so a rejected or aborted one records nothing.
        async move {
            ctx.state.set(TODOS, serde_json::json!(todos))?;
            Ok(format!("Todos updated. {count} items."))
        }
    }
}

#[cfg(test)]
#[path = "todo_tests.rs"]
mod tests;
