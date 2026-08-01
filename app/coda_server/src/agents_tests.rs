use std::pin::Pin;
use std::sync::Arc;

use coda_core::tool::ToolResult;

use super::*;

/// A bare prebuilt tool with a fixed name, standing in for an MCP tool.
struct FakeTool {
    name: String,
    schema: serde_json::Value,
}

impl FakeTool {
    fn boxed(name: &str) -> Box<dyn ToolObject> {
        Box::new(FakeTool {
            name: name.to_string(),
            schema: serde_json::json!({}),
        })
    }
}

impl ToolObject for FakeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "fake"
    }
    fn parameter_schema(&self) -> &serde_json::Value {
        &self.schema
    }
    fn execute(
        self: Arc<Self>,
        _params: String,
        _ctx: coda_core::tool::ToolCallContext,
    ) -> Pin<Box<dyn Future<Output = ToolResult<String>> + Send>> {
        Box::pin(async { Ok(String::new()) })
    }
}

/// A registry preloaded with two `mcp__example__*` tools and one other.
fn registry_with_mcp() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.insert(FakeTool::boxed("mcp__example__search"));
    registry.insert(FakeTool::boxed("mcp__example__extract"));
    registry.insert(FakeTool::boxed("mcp__other__list"));
    registry
}

fn write_agent(dir: &Path, name: &str, content: &str) {
    let agent_dir = dir.join(".coda").join("agents").join(name);
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("AGENT.md"), content).unwrap();
}

fn write_root_agent(dir: &Path, content: &str) {
    let agents_dir = dir.join(".coda").join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(agents_dir.join("AGENT.md"), content).unwrap();
}

/// Build a team rooted at `/ws` with no per-agent workspaces or knowledge —
/// enough for the structural/tool-resolution tests below.
fn build_team(
    registry: &ToolRegistry,
    files: Vec<AgentFile>,
    root_tools: Option<Vec<String>>,
    root_subagents: Option<Vec<String>>,
) -> Result<AgentTeam, LoadError> {
    build_agent_team(
        "/ws",
        SharedSystemPrompt::new("root"),
        &HashMap::new(),
        &HashMap::new(),
        registry,
        files,
        root_tools,
        root_subagents,
    )
}

#[test]
fn no_config_dir_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_agent_files(dir.path()).unwrap().is_empty());
}

#[test]
fn parses_workspace_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "scoped",
        "---\ndescription: x\nmode: stateless\nworkspace: ./sub\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    assert_eq!(files[0].workspace(), Some("./sub"));
}

#[test]
fn resolve_workspace_absent_inherits_root() {
    let root = tempfile::tempdir().unwrap();
    let root_str = root.path().to_string_lossy();
    assert_eq!(
        resolve_agent_workspace("a", &root_str, None).unwrap(),
        *root_str
    );
}

#[test]
fn resolve_workspace_relative_joins_root() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let root_str = root.path().to_string_lossy();
    let resolved = resolve_agent_workspace("a", &root_str, Some("sub")).unwrap();
    assert_eq!(resolved, sub.canonicalize().unwrap().to_string_lossy());
}

#[test]
fn resolve_workspace_trims_incidental_whitespace() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let root_str = root.path().to_string_lossy();
    let resolved = resolve_agent_workspace("a", &root_str, Some("  sub  ")).unwrap();
    assert_eq!(resolved, sub.canonicalize().unwrap().to_string_lossy());
}

#[test]
fn resolve_workspace_blank_inherits_root() {
    let root = tempfile::tempdir().unwrap();
    let root_str = root.path().to_string_lossy();
    assert_eq!(
        resolve_agent_workspace("a", &root_str, Some("   ")).unwrap(),
        *root_str
    );
}

#[test]
fn resolve_workspace_absolute_is_used() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let other_str = other.path().to_string_lossy().into_owned();
    let resolved =
        resolve_agent_workspace("a", &root.path().to_string_lossy(), Some(&other_str)).unwrap();
    assert_eq!(
        resolved,
        other.path().canonicalize().unwrap().to_string_lossy()
    );
}

#[test]
fn resolve_workspace_missing_dir_errors() {
    let root = tempfile::tempdir().unwrap();
    let result = resolve_agent_workspace("a", &root.path().to_string_lossy(), Some("nope"));
    assert!(matches!(result, Err(LoadError::InvalidWorkspace { .. })));
}

#[test]
fn resolve_workspace_file_not_dir_errors() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "x").unwrap();
    let result = resolve_agent_workspace("a", &root.path().to_string_lossy(), Some("file.txt"));
    assert!(matches!(result, Err(LoadError::InvalidWorkspace { .. })));
}

#[test]
fn parses_valid_agent() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "explore",
        "---\ndescription: explores\nmode: stateless\ntools: [read_file, grep]\n---\nYou explore.",
    );
    let files = load_agent_files(dir.path()).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].name, "explore");
    assert_eq!(files[0].mode, SubAgentMode::Stateless);
    assert_eq!(files[0].tools, vec!["read_file", "grep"]);
    assert_eq!(files[0].system_prompt, "You explore.");
}

#[test]
fn parses_model_override() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "deep",
        "---\ndescription: reasons hard\nmode: stateless\nmodel: \"deepseek:deepseek-reasoner\"\nreasoning_effort: high\n---\nYou reason.",
    );
    let files = load_agent_files(dir.path()).unwrap();
    assert_eq!(files[0].model(), Some("deepseek:deepseek-reasoner"));
    assert_eq!(files[0].reasoning_effort(), Some("high".to_string()));
}

#[test]
fn model_override_defaults_to_none() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "plain",
        "---\ndescription: x\nmode: stateless\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    assert_eq!(files[0].model(), None);
    assert_eq!(files[0].reasoning_effort(), None);
}

#[test]
fn reserved_dir_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "coda",
        "---\ndescription: x\nmode: stateful\n---\nbody",
    );
    assert!(matches!(
        load_agent_files(dir.path()),
        Err(LoadError::ReservedName(_))
    ));
}

#[test]
fn empty_body_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "memo",
        "---\ndescription: x\nmode: stateful\n---\n",
    );
    assert!(matches!(
        load_agent_files(dir.path()),
        Err(LoadError::Parse { .. })
    ));
}

#[test]
fn referencing_reserved_subagent_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "boss",
        "---\ndescription: x\nmode: stateful\nsubagents: [coda]\n---\nbody",
    );
    assert!(matches!(
        load_agent_files(dir.path()),
        Err(LoadError::ReservedName(_))
    ));
}

#[test]
fn unknown_tool_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "explore",
        "---\ndescription: x\nmode: stateless\ntools: [no_such_tool]\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    let registry = ToolRegistry::new();
    let result = build_team(&registry, files, None, None);
    assert!(matches!(result, Err(LoadError::UnknownTool { .. })));
}

#[test]
fn roots_exclude_referenced_agents() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "boss",
        "---\ndescription: x\nmode: stateful\nsubagents: [worker]\n---\nbody",
    );
    write_agent(
        dir.path(),
        "worker",
        "---\ndescription: x\nmode: stateless\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    let registry = ToolRegistry::new();
    let team = build_team(&registry, files, None, None).unwrap();

    // Only `boss` is a direct sub-agent of coda; `worker` hangs under boss.
    assert_eq!(team.root().subagents, vec!["boss".to_string()]);
}

#[test]
fn self_referencing_agent_attaches_to_coda() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "loop",
        "---\ndescription: x\nmode: stateful\nsubagents: [loop]\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
    // A self-loop doesn't count as "referenced by another agent", so `loop`
    // is still a root under coda (and thus reachable), not orphaned.
    assert_eq!(team.root().subagents, vec!["loop".to_string()]);
}

#[test]
fn cyclic_agents_are_dropped_as_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        "---\ndescription: x\nmode: stateful\nsubagents: [b]\n---\nbody",
    );
    write_agent(
        dir.path(),
        "b",
        "---\ndescription: x\nmode: stateful\nsubagents: [a]\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    // `a` and `b` reference each other but neither is reachable from the
    // root, so both are dropped (with a warning) and the team still builds.
    let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
    assert!(team.root().subagents.is_empty());
}

#[test]
fn tool_named_like_subagent_conflicts() {
    // A root-level agent named `grep` collides with coda's built-in `grep`
    // tool, since coda exposes both in one namespace.
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "grep",
        "---\ndescription: x\nmode: stateless\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    // Team construction catches the conflict from spec metadata alone — no
    // build.
    let result = build_team(&ToolRegistry::new(), files, None, None);
    assert!(matches!(
        result,
        Err(LoadError::Build(BuildError::NameConflict { .. }))
    ));
}

#[test]
fn shared_subagent_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "a",
        "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
    );
    write_agent(
        dir.path(),
        "b",
        "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
    );
    write_agent(
        dir.path(),
        "shared",
        "---\ndescription: x\nmode: stateless\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    let team = build_team(&ToolRegistry::new(), files, None, None).unwrap();
    let agents = team.build(".", coda_tools::shared_file_locks());
    assert!(agents.contains_key("shared"));
    assert!(agents.contains_key("a"));
    assert!(agents.contains_key("b"));
}

#[test]
fn no_root_agent_file_is_all_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = load_root_agent_file(dir.path()).unwrap();
    assert!(root.tools.is_none());
    assert!(root.subagents.is_none());
    assert!(root.system_prompt.is_none());
}

#[test]
fn root_agent_file_parses_overrides_and_body() {
    let dir = tempfile::tempdir().unwrap();
    write_root_agent(
        dir.path(),
        "---\ntools: [shell, read_file]\nsubagents: [explore]\n---\nYou are coda.",
    );
    let root = load_root_agent_file(dir.path()).unwrap();
    assert_eq!(
        root.tools,
        Some(vec!["shell".to_string(), "read_file".to_string()])
    );
    assert_eq!(root.subagents, Some(vec!["explore".to_string()]));
    assert_eq!(root.system_prompt.as_deref(), Some("You are coda."));
}

#[test]
fn root_agent_file_empty_body_is_none() {
    let dir = tempfile::tempdir().unwrap();
    write_root_agent(dir.path(), "---\ntools: []\n---\n");
    let root = load_root_agent_file(dir.path()).unwrap();
    // Explicit empty list overrides to "no tools"; empty body falls back.
    assert_eq!(root.tools, Some(vec![]));
    assert!(root.system_prompt.is_none());
}

#[test]
fn root_subagents_override_enables_root_sharing() {
    // `shared` is referenced by `boss`, so the default heuristic would hide
    // it from coda. An explicit root `subagents` list mounts it under coda
    // too — the same agent shared by root and another agent.
    let dir = tempfile::tempdir().unwrap();
    write_agent(
        dir.path(),
        "boss",
        "---\ndescription: x\nmode: stateful\nsubagents: [shared]\n---\nbody",
    );
    write_agent(
        dir.path(),
        "shared",
        "---\ndescription: x\nmode: stateless\n---\nbody",
    );
    let files = load_agent_files(dir.path()).unwrap();
    let team = build_team(
        &ToolRegistry::new(),
        files,
        None,
        Some(vec!["boss".into(), "shared".into()]),
    )
    .unwrap();
    assert_eq!(
        team.root().subagents,
        vec!["boss".to_string(), "shared".to_string()]
    );
}

#[test]
fn root_tools_override_unknown_tool_errors() {
    let dir = tempfile::tempdir().unwrap();
    let files = load_agent_files(dir.path()).unwrap();
    let result = build_team(
        &ToolRegistry::new(),
        files,
        Some(vec!["no_such_tool".into()]),
        None,
    );
    assert!(matches!(
        result,
        Err(LoadError::UnknownTool { agent, .. }) if agent == ROOT_AGENT_NAME
    ));
}

#[test]
fn tool_pattern_expands_to_matching_prebuilt_tools() {
    let registry = registry_with_mcp();
    let tools = resolve_tools(
        &registry,
        "explore",
        &["read_file".to_string(), "mcp__example__*".to_string()],
    )
    .unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    // Literal first, then the two example tools (sorted); other excluded.
    assert_eq!(
        names,
        vec!["read_file", "mcp__example__extract", "mcp__example__search"]
    );
}

#[test]
fn tool_pattern_dedups_against_literal_overlap() {
    let registry = registry_with_mcp();
    // `mcp__example__search` named both explicitly and via the pattern must
    // appear once, or the downstream namespace check would reject it.
    let tools = resolve_tools(
        &registry,
        "explore",
        &[
            "mcp__example__search".to_string(),
            "mcp__example__*".to_string(),
        ],
    )
    .unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names, vec!["mcp__example__search", "mcp__example__extract"]);
}

#[test]
fn tool_pattern_matching_nothing_is_not_an_error() {
    let registry = registry_with_mcp();
    let tools = resolve_tools(&registry, "explore", &["mcp__nope__*".to_string()]).unwrap();
    assert!(tools.is_empty());
}

#[test]
fn bare_star_is_not_a_wildcard() {
    // A bare `*` is not a pattern (omit `tools` to get everything); it has
    // no non-empty prefix, so it resolves like a literal and is unknown.
    let registry = registry_with_mcp();
    let result = resolve_tools(&registry, "explore", &["*".to_string()]);
    assert!(matches!(
        result,
        Err(LoadError::UnknownTool { tool, .. }) if tool == "*"
    ));
}

#[test]
fn root_subagents_referencing_reserved_name_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_root_agent(dir.path(), "---\nsubagents: [coda]\n---\nbody");
    assert!(matches!(
        load_root_agent_file(dir.path()),
        Err(LoadError::ReservedName(_))
    ));
}
