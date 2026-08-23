use std::pin::Pin;
use std::sync::Arc;

use coda_core::tool::{ToolCallContext, ToolObject, ToolResult, ToolWrapper};

use crate::locks::KeyedLock;
use crate::{
    EditFileTool, GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool,
    WriteFileTool, WriteTodosTool,
};
use coda_ptc::RunJavaScriptTool;

/// Runtime context for building tools.
#[derive(Clone)]
pub struct BuildContext {
    pub workspace_dir: String,
    /// Shared by *every* agent and session in the process, or the file tools
    /// serialize against registries nobody else consults and exclude nothing.
    /// Defaults to [`shared_file_locks`].
    pub file_locks: Arc<KeyedLock<String>>,
}

impl BuildContext {
    /// A standalone context, holding — on purpose — the process-wide file lock
    /// registry.
    pub fn new(workspace_dir: impl Into<String>) -> Self {
        BuildContext {
            workspace_dir: workspace_dir.into(),
            file_locks: crate::locks::shared_file_locks(),
        }
    }
}

/// A factory for creating tool instances.
///
/// `name` is lightweight metadata — the name the built tool will report —
/// available without constructing the tool. It lets callers validate things
/// like tool/sub-agent namespace conflicts without paying `build`'s cost or
/// triggering any side effects it may have. Implementations must keep `name`
/// consistent with `build(..).name()`.
pub trait ToolSpec: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject>;
}

// --- Built-in tool specs ---

pub struct ShellToolSpec;

impl ToolSpec for ShellToolSpec {
    fn name(&self) -> &str {
        "shell"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ShellTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct ReadFileToolSpec;

impl ToolSpec for ReadFileToolSpec {
    fn name(&self) -> &str {
        "read_file"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadFileTool::new()))
    }
}

pub struct WriteFileToolSpec;

impl ToolSpec for WriteFileToolSpec {
    fn name(&self) -> &str {
        "write_file"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteFileTool::new(
            ctx.file_locks.clone(),
        )))
    }
}

pub struct EditFileToolSpec;

impl ToolSpec for EditFileToolSpec {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(EditFileTool::new(ctx.file_locks.clone())))
    }
}

pub struct ListDirectoryToolSpec;

impl ToolSpec for ListDirectoryToolSpec {
    fn name(&self) -> &str {
        "ls"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ListDirectoryTool::new()))
    }
}

pub struct GrepToolSpec;

impl ToolSpec for GrepToolSpec {
    fn name(&self) -> &str {
        "grep"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GrepTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct GlobToolSpec;

impl ToolSpec for GlobToolSpec {
    fn name(&self) -> &str {
        "glob"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(GlobTool::new(ctx.workspace_dir.clone())))
    }
}

pub struct ReadTodosToolSpec;

impl ToolSpec for ReadTodosToolSpec {
    fn name(&self) -> &str {
        "read_todos"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(ReadTodosTool::new()))
    }
}

pub struct WriteTodosToolSpec;

impl ToolSpec for WriteTodosToolSpec {
    fn name(&self) -> &str {
        "write_todos"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(WriteTodosTool::new()))
    }
}

pub struct RunJavaScriptToolSpec;

impl ToolSpec for RunJavaScriptToolSpec {
    fn name(&self) -> &str {
        coda_ptc::RUN_JAVASCRIPT_TOOL_NAME
    }

    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(RunJavaScriptTool::default()))
    }
}

/// Wraps a pre-built `ToolObject` as a `ToolSpec`. Each call to `build`
/// returns a shared wrapper around the same underlying tool. Cloning shares
/// the same underlying tool, so one prebuilt tool can be handed to multiple
/// agents without rebuilding (e.g. a single MCP connection, many agents).
#[derive(Clone)]
pub struct PrebuiltToolSpec(Arc<dyn ToolObject>);

impl PrebuiltToolSpec {
    pub fn new(tool: Box<dyn ToolObject>) -> Self {
        PrebuiltToolSpec(Arc::from(tool))
    }
}

impl ToolSpec for PrebuiltToolSpec {
    fn name(&self) -> &str {
        self.0.name()
    }
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
        ctx: ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        self.0.clone().execute(params, ctx)
    }
}

/// Returns builtin tool specs for a standard agent.
pub fn builtin_specs() -> Vec<Box<dyn ToolSpec>> {
    BUILTIN_TOOL_NAMES
        .iter()
        .map(|name| spec_by_name(name).expect("builtin name resolves"))
        .collect()
}

/// Names of all builtin tools, in canonical order.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "shell",
    "read_file",
    "write_file",
    "edit_file",
    "ls",
    "grep",
    "glob",
    "read_todos",
    "write_todos",
    "run_javascript",
];

/// Resolves a builtin tool name to a fresh [`ToolSpec`]. Returns `None` for any
/// name that is not a builtin, letting callers fall back to other tool sources
/// (e.g. MCP) or report an unknown-tool error.
pub fn spec_by_name(name: &str) -> Option<Box<dyn ToolSpec>> {
    Some(match name {
        "shell" => Box::new(ShellToolSpec),
        "read_file" => Box::new(ReadFileToolSpec),
        "write_file" => Box::new(WriteFileToolSpec),
        "edit_file" => Box::new(EditFileToolSpec),
        "ls" => Box::new(ListDirectoryToolSpec),
        "grep" => Box::new(GrepToolSpec),
        "glob" => Box::new(GlobToolSpec),
        "read_todos" => Box::new(ReadTodosToolSpec),
        "write_todos" => Box::new(WriteTodosToolSpec),
        "run_javascript" => Box::new(RunJavaScriptToolSpec),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ToolSpec::name` is metadata used for validation without building; it must
    /// stay consistent with the name the built tool actually reports.
    #[test]
    fn builtin_spec_name_matches_built_tool() {
        let ctx = BuildContext::new(".");
        for name in BUILTIN_TOOL_NAMES {
            let spec = spec_by_name(name).expect("builtin resolves");
            assert_eq!(spec.name(), *name, "spec_by_name key vs ToolSpec::name");
            assert_eq!(
                spec.name(),
                spec.build(&ctx).name(),
                "ToolSpec::name vs built tool name for '{name}'"
            );
        }
    }
}
