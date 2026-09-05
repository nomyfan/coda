use std::pin::Pin;
use std::sync::Arc;

use coda_core::tool::{ToolCallContext, ToolObject, ToolResult, ToolWrapper};

use coda_process::BackgroundTasks;

use crate::locks::KeyedLock;
use crate::{
    EditFileTool, GlobTool, GrepTool, ListDirectoryTool, ReadFileTool, ReadTodosTool, ShellTool,
    TaskKillTool, TaskOutputTool, WriteFileTool, WriteTodosTool,
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
    /// Name of the agent the tools are built for; echoed in the metadata of
    /// background tasks it starts.
    pub agent_name: String,
    /// Shared by every agent in one session, and the one thing that decides
    /// whether background work exists at all: `None` takes both
    /// [`background_specs`] and `shell`'s `run_in_background` with it. That is
    /// a capability, not a permission — backgrounding changes how long a
    /// command may run, so the call still faces the usual approval policy.
    pub background: Option<Arc<BackgroundTasks>>,
}

impl BuildContext {
    /// A standalone context, holding — on purpose — the process-wide file lock
    /// registry.
    ///
    /// Background work is off: building specs one at a time skips
    /// [`background_specs`], so a task started here could not be followed up on.
    pub fn new(workspace_dir: impl Into<String>) -> Self {
        BuildContext {
            workspace_dir: workspace_dir.into(),
            file_locks: crate::locks::shared_file_locks(),
            agent_name: "coda".into(),
            background: None,
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
        Box::new(ToolWrapper::from(ShellTool::new(
            ctx.workspace_dir.clone(),
            ctx.agent_name.clone(),
            ctx.background.clone(),
        )))
    }
}

/// Both specs carry the registry instead of reading it off the context:
/// [`background_specs`] is their only constructor and only runs when there is
/// one, so neither can exist without a registry.
pub(crate) struct TaskOutputToolSpec(Arc<BackgroundTasks>);

impl ToolSpec for TaskOutputToolSpec {
    fn name(&self) -> &str {
        "task_output"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(TaskOutputTool::new(self.0.clone())))
    }
}

pub(crate) struct TaskKillToolSpec(Arc<BackgroundTasks>);

impl ToolSpec for TaskKillToolSpec {
    fn name(&self) -> &str {
        "task_kill"
    }
    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        Box::new(ToolWrapper::from(TaskKillTool::new(self.0.clone())))
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

/// The background follow-up tools, for a session that has a registry — empty
/// otherwise.
///
/// Not declarable in `tools` and not granted per agent: whether there is
/// anything to follow up on is the only question, and `shell`'s
/// `run_in_background` answers it the same way, so the three appear and
/// disappear together.
pub fn background_specs(background: Option<&Arc<BackgroundTasks>>) -> Vec<Box<dyn ToolSpec>> {
    match background {
        Some(registry) => vec![
            Box::new(TaskOutputToolSpec(registry.clone())),
            Box::new(TaskKillToolSpec(registry.clone())),
        ],
        None => Vec::new(),
    }
}

/// Names of the builtin tools an agent may name in `tools`, in canonical
/// order. The background tools are absent on purpose — see
/// [`background_specs`].
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

    /// Building specs one at a time skips the injection point, so the shell a
    /// standalone context builds must not offer to background anything.
    #[test]
    fn a_standalone_context_builds_a_foreground_only_shell() {
        let shell = ShellToolSpec.build(&BuildContext::new("."));
        assert!(
            shell.parameter_schema()["properties"]
                .get("run_in_background")
                .is_none(),
            "{}",
            shell.parameter_schema()
        );
    }

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
