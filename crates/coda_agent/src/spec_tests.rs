use std::pin::Pin;
use std::sync::Mutex as StdMutex;

use coda_core::llm::{Message, MessageId, TurnId, UserMessage};
use coda_core::tool::{ToolCallContext, ToolObject, ToolResult};

use super::*;

fn spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.into(),
        description: String::new(),
        system_prompt: "".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    }
}

#[tokio::test]
async fn processes_share_the_program_but_not_history_or_tool_state() {
    let team = AgentTeam::new(spec("worker"), vec![]).unwrap();
    let programs = team.build(".", coda_tools::shared_file_locks(), None);
    let program = programs["worker"].clone();
    let first = crate::Process::new(crate::ProcessId::new(), program.clone());
    let second = crate::Process::new(crate::ProcessId::new(), program);
    assert!(Arc::ptr_eq(&first.program, &second.program));
    let message = MessageId::new();
    first
        .restore_history(vec![crate::HistoryEntry {
            turn_id: TurnId::from(message),
            message: Message::User(UserMessage::text(message, "private")),
            state: [("notes".into(), serde_json::json!("private"))].into(),
        }])
        .await;
    assert_eq!(first.history().await.len(), 1);
    assert_eq!(
        first.state_snapshot().await["notes"],
        serde_json::json!("private")
    );
    assert!(second.history().await.is_empty());
    assert!(second.state_snapshot().await.is_empty());
    let other_session = team.build(".", coda_tools::shared_file_locks(), None);
    assert!(!Arc::ptr_eq(&first.program, &other_session["worker"]));
}

/// A tool that records the workspace it was built with, for asserting that
/// each agent's tools are rooted at its own workspace.
struct RecordingTool;
impl ToolObject for RecordingTool {
    fn name(&self) -> &str {
        "rec"
    }
    fn description(&self) -> &str {
        "records"
    }
    fn parameter_schema(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({}))
    }
    fn execute(
        self: Arc<Self>,
        _params: String,
        _ctx: ToolCallContext,
    ) -> Pin<Box<dyn std::future::Future<Output = ToolResult<String>> + Send>> {
        Box::pin(async { Ok(String::new()) })
    }
}

struct RecordingToolSpec {
    seen: Arc<StdMutex<Vec<String>>>,
}
impl ToolSpec for RecordingToolSpec {
    fn name(&self) -> &str {
        "rec"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        self.seen.lock().unwrap().push(ctx.workspace_dir.clone());
        Box::new(RecordingTool)
    }
}

/// Captures what each tool build sees: agent, whether background work is
/// possible, and which registry.
struct BackgroundProbeSpec {
    seen: Arc<StdMutex<Vec<(String, bool, usize)>>>,
}
impl ToolSpec for BackgroundProbeSpec {
    fn name(&self) -> &str {
        "probe"
    }
    fn build(&self, ctx: &BuildContext) -> Box<dyn ToolObject> {
        self.seen.lock().unwrap().push((
            ctx.agent_name.clone(),
            ctx.background.is_some(),
            ctx.background
                .as_ref()
                .map_or(0, |registry| Arc::as_ptr(registry) as usize),
        ));
        Box::new(RecordingTool)
    }
}

/// The follow-up tools are the session's, not an agent's: nobody declares
/// them and every agent gets them, over one shared registry.
#[test]
fn background_tools_are_injected_for_every_agent_and_share_one_registry() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let probe =
        || Box::new(BackgroundProbeSpec { seen: seen.clone() }) as Box<dyn coda_tools::ToolSpec>;
    let root = AgentSpec {
        tools: vec![probe()],
        subagents: vec!["sub".into()],
        ..spec("coda")
    };
    let sub = AgentSpec {
        tools: vec![probe()],
        ..spec("sub")
    };
    let team = AgentTeam::new(root, vec![sub]).unwrap();
    let background = Arc::new(BackgroundTasks::temporary().unwrap());
    let agents = team.build(
        ".",
        coda_tools::shared_file_locks(),
        Some(background.clone()),
    );

    for name in ["coda", "sub"] {
        let tools = &agents[name].tools;
        assert!(
            tools.get("task_output").is_some(),
            "{name} has no task_output"
        );
        assert!(tools.get("task_kill").is_some(), "{name} has no task_kill");
    }
    let mut got = seen.lock().unwrap().clone();
    got.sort();
    let registry = Arc::as_ptr(&background) as usize;
    assert_eq!(
        got,
        vec![
            ("coda".to_string(), true, registry),
            ("sub".to_string(), true, registry),
        ]
    );
}

/// No storage, nothing to follow up on: the two tools are never injected
/// and `shell` stops offering to background anything. One condition,
/// three surfaces.
#[test]
fn without_background_storage_the_task_tools_are_never_registered() {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let tool = |name: &str| coda_tools::spec_by_name(name).expect("builtin");
    let root = AgentSpec {
        tools: vec![
            Box::new(BackgroundProbeSpec { seen: seen.clone() }),
            tool("shell"),
        ],
        ..spec("coda")
    };
    let team = AgentTeam::new(root, vec![]).unwrap();

    let agents = team.build(".", coda_tools::shared_file_locks(), None);

    let tools = &agents["coda"].tools;
    assert!(tools.get("task_output").is_none());
    assert!(tools.get("task_kill").is_none());
    assert!(
        tools.get("shell").is_some(),
        "an unrelated tool was dropped"
    );
    assert_eq!(
        seen.lock().unwrap().first().map(|(_, allowed, _)| *allowed),
        Some(false),
        "shell was offered a follow-up kit that was not registered"
    );
}

/// The injected names are reserved, so nothing can register them by hand.
#[test]
fn rejects_a_spec_claiming_an_injected_background_tool_name() {
    for name in ["task_output", "task_kill"] {
        let root = AgentSpec {
            tools: vec![Box::new(NamedSpec(name))],
            ..spec("coda")
        };
        assert!(matches!(
            AgentTeam::new(root, vec![]),
            Err(BuildError::ReservedToolName { name: claimed, .. }) if claimed == name
        ));
    }
}

struct NamedSpec(&'static str);

impl ToolSpec for NamedSpec {
    fn name(&self) -> &str {
        self.0
    }

    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        unreachable!("reserved tool names are rejected before build")
    }
}

struct ReservedToolSpec;

impl ToolSpec for ReservedToolSpec {
    fn name(&self) -> &str {
        coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
    }

    fn build(&self, _ctx: &BuildContext) -> Box<dyn ToolObject> {
        unreachable!("reserved tool names are rejected before build")
    }
}

#[test]
fn rejects_reserved_synthetic_tool_name_without_a_runner() {
    let root = AgentSpec {
        tools: vec![Box::new(ReservedToolSpec)],
        ..spec("coda")
    };

    assert!(matches!(
        AgentTeam::new(root, vec![]),
        Err(BuildError::ReservedToolName { agent, name })
            if agent == "coda" && name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
    ));
}

#[test]
fn rejects_reserved_synthetic_tool_name_on_a_reachable_subagent() {
    let root = AgentSpec {
        subagents: vec!["sub".into()],
        ..spec("coda")
    };
    let sub = AgentSpec {
        tools: vec![Box::new(ReservedToolSpec)],
        ..spec("sub")
    };

    assert!(matches!(
        AgentTeam::new(root, vec![sub]),
        Err(BuildError::ReservedToolName { agent, name })
            if agent == "sub" && name == coda_tools::LIST_JAVASCRIPT_TOOLS_TOOL_NAME
    ));
}

#[test]
fn build_roots_tools_at_per_agent_workspace() {
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let mk = || Box::new(RecordingToolSpec { seen: seen.clone() }) as Box<dyn ToolSpec>;
    let root = AgentSpec {
        tools: vec![mk()],
        subagents: vec!["sub".into()],
        ..spec("coda")
    };
    let sub = AgentSpec {
        tools: vec![mk()],
        ..spec("sub")
    };
    let team = AgentTeam::new(root, vec![sub])
        .unwrap()
        .with_agent_workspaces(HashMap::from([("sub".to_string(), "/sub".to_string())]));

    team.build(
        "/root",
        coda_tools::shared_file_locks(),
        Some(Arc::new(BackgroundTasks::temporary().unwrap())),
    );

    let mut got = seen.lock().unwrap().clone();
    got.sort();
    // Root falls back to the default workspace; `sub` uses its override.
    assert_eq!(got, vec!["/root".to_string(), "/sub".to_string()]);
}

#[test]
fn rejects_subagent_name_overflowing_prefixed_tool_limit() {
    let too_long = "a".repeat(MAX_TOOL_NAME_LEN - SUBAGENT_TOOL_PREFIX.len() + 1);
    let root = AgentSpec {
        subagents: vec![too_long.clone()],
        ..spec("coda")
    };
    let result = AgentTeam::new(root, vec![spec(&too_long)]);
    assert!(matches!(
        result,
        Err(BuildError::SubagentNameTooLong { .. })
    ));
}

#[test]
fn ignores_unreachable_subagent_name_overflowing_prefixed_tool_limit() {
    let too_long = "a".repeat(MAX_TOOL_NAME_LEN - SUBAGENT_TOOL_PREFIX.len() + 1);
    assert!(AgentTeam::new(spec("coda"), vec![spec(&too_long)]).is_ok());
}

#[test]
fn accepts_subagent_name_at_the_prefixed_tool_limit() {
    let max = "a".repeat(MAX_TOOL_NAME_LEN - SUBAGENT_TOOL_PREFIX.len());
    let root = AgentSpec {
        subagents: vec![max.clone()],
        ..spec("coda")
    };
    assert!(AgentTeam::new(root, vec![spec(&max)]).is_ok());
}

/// A freshly built agent has run nothing, so it is in no turn — and asking
/// must not start one. The driver asks on entry, before the thread's first
/// prompt has landed; answering by minting a turn both reported an
/// invariant break that had not happened and left the thread stamped with
/// a turn no message belongs to.
#[tokio::test]
async fn a_fresh_agent_is_in_no_turn_and_asking_does_not_open_one() {
    let agents = AgentTeam::new(spec("coda"), vec![])
        .expect("valid team")
        .build(
            "/root",
            coda_tools::shared_file_locks(),
            Some(Arc::new(BackgroundTasks::temporary().unwrap())),
        );
    let agent = &agents["coda"];

    let agent = crate::Process::new(crate::ProcessId::new(), agent.clone());
    assert_eq!(agent.current_turn().await, None);
    // Twice: the first ask must leave nothing behind for the second to find.
    assert_eq!(agent.current_turn().await, None);
    assert!(agent.history().await.is_empty());

    let turn = TurnId::from(MessageId::new());
    agent
        .add_opening_message(
            turn,
            Message::User(UserMessage::text(MessageId::new(), "inspect")),
        )
        .await;

    assert_eq!(agent.current_turn().await, Some(turn));
}
