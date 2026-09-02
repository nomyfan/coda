mod fs;
mod glob;
mod grep;
mod locks;
mod process;
mod shell;
mod spec;
mod task;
mod todo;

pub use coda_ptc::{
    LIST_JAVASCRIPT_TOOLS_TOOL_NAME, PROGRAMMATIC_TOOL_NAMES, RUN_JAVASCRIPT_TOOL_NAME,
    available_tools_message, list_javascript_tools_definition, run_javascript_definition,
    tool_unavailable_message,
};
pub use fs::{EditFileTool, ListDirectoryTool, ReadFileTool, WriteFileTool};
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use locks::{KeyedGuard, KeyedLock, shared_file_locks};
pub use shell::ShellTool;
pub use spec::{
    BUILTIN_TOOL_NAMES, BuildContext, EditFileToolSpec, GlobToolSpec, GrepToolSpec,
    ListDirectoryToolSpec, PrebuiltToolSpec, ReadFileToolSpec, ReadTodosToolSpec,
    RunJavaScriptToolSpec, ShellToolSpec, ToolSpec, WriteFileToolSpec, WriteTodosToolSpec,
    background_specs, builtin_specs, spec_by_name,
};
pub use task::{TaskKillTool, TaskOutputTool};
pub use todo::{ReadTodosTool, TodoItem, WriteTodosTool};

/// Provider-visible synthetic names that ToolSpec implementations may not
/// claim: the runtime injects these itself, so a spec claiming one would be
/// registered where the real tool is absent, and replaced where it is not.
pub const SYNTHETIC_RESERVED_TOOL_NAMES: &[&str] =
    &[LIST_JAVASCRIPT_TOOLS_TOOL_NAME, "task_output", "task_kill"];
