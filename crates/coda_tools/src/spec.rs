use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

use coda_core::tool::{ToolObject, ToolResult, ToolWrapper};

use crate::todo::TodoItem;
use crate::{
    GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool, WriteFileTool,
    WriteTodosTool,
};

/// Runtime context for building tools.
#[derive(Clone)]
pub struct BuildContext {
    pub workspace_dir: String,
    pub todo_store: Option<Arc<Mutex<Vec<TodoItem>>>>,
}

impl BuildContext {
    pub fn new(workspace_dir: impl Into<String>) -> Self {
        BuildContext {
            workspace_dir: workspace_dir.into(),
            todo_store: None,
        }
    }
}

/// A factory for creating tool instances.
pub trait ToolSpec: Send + Sync {
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject>;
}

// --- Built-in tool specs ---

pub struct ShellToolSpec;

impl ToolSpec for ShellToolSpec {
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ShellTool::new()))
    }
}

pub struct ReadFileToolSpec;

impl ToolSpec for ReadFileToolSpec {
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadFileTool::new()))
    }
}

pub struct WriteFileToolSpec;

impl ToolSpec for WriteFileToolSpec {
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteFileTool::new()))
    }
}

pub struct ListDirectoryToolSpec;

impl ToolSpec for ListDirectoryToolSpec {
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ListDirectoryTool::new()))
    }
}

pub struct GrepToolSpec;

impl ToolSpec for GrepToolSpec {
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GrepTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct GlobToolSpec;

impl ToolSpec for GlobToolSpec {
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GlobTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct ReadTodosToolSpec;

impl ToolSpec for ReadTodosToolSpec {
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadTodosTool::new(
            ctx.todo_store.clone().expect("todo_store must be set"),
        )))
    }
}

pub struct WriteTodosToolSpec;

impl ToolSpec for WriteTodosToolSpec {
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteTodosTool::new(
            ctx.todo_store.clone().expect("todo_store must be set"),
        )))
    }
}

/// Wraps a pre-built `ToolObject` as a `ToolSpec`. The object is yielded on
/// the first call to `build`; subsequent calls panic (each spec instance
/// should only be built once during agent construction).
pub struct PrebuiltToolSpec(Arc<dyn ToolObject>);

impl PrebuiltToolSpec {
    pub fn new(tool: Box<dyn ToolObject>) -> Self {
        PrebuiltToolSpec(Arc::from(tool))
    }
}

impl ToolSpec for PrebuiltToolSpec {
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(SharedToolObject(self.0.clone()))
    }
}

struct SharedToolObject(Arc<dyn ToolObject>);

impl ToolObject for SharedToolObject {
    fn name(&self) -> &str {
        self.0.name()
    }

    fn description(&self) -> &str {
        self.0.description()
    }

    fn parameter_schema(&self) -> &serde_json::Value {
        self.0.parameter_schema()
    }

    fn execute(
        self: Arc<Self>,
        params: String,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        self.0.clone().execute(params)
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
