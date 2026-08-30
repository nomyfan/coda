//! Session lifecycle tests.
//!
//! Exercise the full `Session` API (builder → open → send → recv → shutdown)
//! with a fake LLM provider, covering real built-in tools, multi-turn
//! conversations, sub-agent delegation, session resume, and approval flows.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use coda_agent::runtime::{MemoryStorage, SessionStorage};
use coda_agent::{
    AgentEvent, AgentSpec, AgentTeam, ModelProfile, OpenError, ResumeDecision, RunConfig, Session,
    SessionEvent, SessionStreamItem, Shutdown, SubAgentMode, ToolApprovalMode, ToolCallResolution,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMStreamEvent, Message, MessageId, RequestMessage,
    StreamError, ToolCall, ToolOutput,
};
use futures::{Stream, stream};
use serde_json::json;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// FakeProvider — a mock LLMProvider that routes based on message content
// ---------------------------------------------------------------------------

/// Extracts the text of the last `User` message in a request.
fn last_user_text(messages: &[RequestMessage]) -> &str {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            RequestMessage::User(u) => u.first_text(),
            _ => None,
        })
        .unwrap_or("")
}

/// The most recent result of one named tool, if the thread holds one. Distinct
/// from [`has_tool_results`]: a thread several turns in always holds *some* tool
/// message, so a branch that must fire once per turn has to name its own.
fn tool_result(messages: &[RequestMessage], name: &str) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        RequestMessage::Tool(tool) if tool.name == name => Some(match &tool.output {
            ToolOutput::Ok(text) => text.clone(),
            ToolOutput::Err(err) => format!("error: {err}"),
        }),
        _ => None,
    })
}

/// Returns `true` if the message list contains any `Tool` message.
fn has_tool_results(messages: &[RequestMessage]) -> bool {
    messages
        .iter()
        .any(|m| matches!(m, RequestMessage::Tool(_)))
}

/// Count the number of `User` messages in the request.
fn user_message_count(messages: &[RequestMessage]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, RequestMessage::User(_)))
        .count()
}

fn completed(
    msg: AssistantMessage,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
    Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(Box::new(
        msg,
    )))]))
}

/// Base assistant message for tests; callers override the fields they care
/// about with struct-update syntax (`..assistant()`).
fn assistant() -> AssistantMessage {
    let now = jiff::Timestamp::now();
    AssistantMessage {
        message_id: MessageId::new(),
        content: String::new(),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
        reasoning_continuation: None,
        reasoning_ended_at: None,
        aborted: false,
        started_at: now,
        ended_at: now,
    }
}

/// A fake LLM provider that returns pre-scripted responses based on message
/// content.
///
/// Routing logic uses the last user message text (not system prompt) because
/// the session integration tests use production-style system prompts.
#[derive(Clone, Default)]
struct FakeProvider;

impl coda_core::llm::LLMProvider for FakeProvider {
    fn stream(
        &self,
        request: ChatCompletionRequest,
    ) -> impl Stream<Item = Result<LLMStreamEvent, StreamError>> + Send + '_ {
        let user_text = last_user_text(&request.messages);
        let has_results = has_tool_results(&request.messages);

        // --- Routing ---

        // 1. "simple hello" → pure text reply
        if user_text.contains("simple hello") {
            return completed(AssistantMessage {
                content: "Hello from the agent!".into(),
                ..assistant()
            });
        }

        // 1b. "plan the work" → write_todos, then read them back on the next
        //     turn, so a test can watch state travel through the runtime.
        if user_text.contains("plan the work") {
            // Keyed on *this* tool's result, not on any tool result: by the
            // second turn the thread already holds tool messages from the first.
            if tool_result(&request.messages, "write_todos").is_some() {
                return completed(AssistantMessage {
                    content: "planned".into(),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_write_todos".into(),
                    name: "write_todos".into(),
                    arguments: Some(
                        json!({"todos": [
                            {"title": "parse", "done": true},
                            {"title": "test", "done": false},
                        ]})
                        .to_string(),
                    ),
                }],
                ..assistant()
            });
        }

        // 1c. "what is left" → read_todos, then report what came back.
        if user_text.contains("what is left") {
            if let Some(listed) = tool_result(&request.messages, "read_todos") {
                return completed(AssistantMessage {
                    content: format!("todos: {listed}"),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_read_todos".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..assistant()
            });
        }

        // 2. "read file at <path>" → call read_file, then summarize
        if let Some(path) = user_text.strip_prefix("read file at ") {
            let path = path.trim();
            if has_results {
                let tool_output = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        RequestMessage::Tool(t) if t.name == "read_file" => {
                            if let ToolOutput::Ok(ref s) = t.output {
                                Some(s.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                return completed(AssistantMessage {
                    content: format!("file-content: {tool_output}"),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_read".into(),
                    name: "read_file".into(),
                    arguments: Some(json!({"file_path": path}).to_string()),
                }],
                ..assistant()
            });
        }

        // 3. "write then read <path>" → write_file first, then read_file
        if let Some(path) = user_text.strip_prefix("write then read ") {
            let path = path.trim();

            let has_write = request
                .messages
                .iter()
                .any(|m| matches!(m, RequestMessage::Tool(t) if t.name == "write_file"));
            let has_read = request
                .messages
                .iter()
                .any(|m| matches!(m, RequestMessage::Tool(t) if t.name == "read_file"));

            if has_read {
                let read_output = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        RequestMessage::Tool(t) if t.name == "read_file" => {
                            if let ToolOutput::Ok(ref s) = t.output {
                                Some(s.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                return completed(AssistantMessage {
                    content: format!("round-trip: {read_output}"),
                    ..assistant()
                });
            } else if has_write {
                return completed(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments: Some(json!({"file_path": path}).to_string()),
                    }],
                    ..assistant()
                });
            } else {
                return completed(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call_write".into(),
                        name: "write_file".into(),
                        arguments: Some(
                            json!({"file_path": path, "content": "session-test-data"}).to_string(),
                        ),
                    }],
                    ..assistant()
                });
            }
        }

        // 4. "delegate to explore" → call explore sub-agent tool
        if user_text.contains("delegate to explore") {
            if has_results {
                let explore_output = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        RequestMessage::Tool(t) if t.name == "explore" => {
                            if let ToolOutput::Ok(ref s) = t.output {
                                Some(s.clone())
                            } else {
                                None
                            }
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                return completed(AssistantMessage {
                    content: format!("explore-result: {explore_output}"),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_explore".into(),
                    name: "explore".into(),
                    arguments: Some(r#"{"task":"session probe"}"#.into()),
                }],
                ..assistant()
            });
        }

        // For explore sub-agent: respond with a simple message
        if user_text.contains("session probe") {
            return completed(AssistantMessage {
                content: "explore-done".into(),
                ..assistant()
            });
        }

        // 4b. Delegate to a sub-agent that itself needs tool approval.
        if user_text.contains("delegate approval to explore") {
            if has_results {
                return completed(AssistantMessage {
                    content: "subagent-approval-done".into(),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_explore_approval".into(),
                    name: "explore".into(),
                    arguments: Some(r#"{"task":"subagent approval probe"}"#.into()),
                }],
                ..assistant()
            });
        }

        if user_text.contains("subagent approval probe") {
            if has_results {
                return completed(AssistantMessage {
                    content: "explore-approval-done".into(),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_subagent_todos".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..assistant()
            });
        }

        // 5. Multi-turn: "multi turn start" then "multi turn follow"
        if user_text.contains("multi turn start") {
            return completed(AssistantMessage {
                content: "turn-1-reply".into(),
                ..assistant()
            });
        }
        if user_text.contains("multi turn follow") {
            let count = user_message_count(&request.messages);
            return completed(AssistantMessage {
                content: format!("turn-2-reply (saw {count} user messages)"),
                ..assistant()
            });
        }

        // 6. "resume test start" / "resume test follow"
        if user_text.contains("resume test start") {
            return completed(AssistantMessage {
                content: "session-1-reply".into(),
                ..assistant()
            });
        }
        if user_text.contains("resume test follow") {
            let total = request.messages.len();
            return completed(AssistantMessage {
                content: format!("session-2-reply (history-len: {total})"),
                ..assistant()
            });
        }

        // 7. "approve read_todos" → call read_todos (requires approval)
        if user_text.contains("approve read_todos") {
            if has_results {
                return completed(AssistantMessage {
                    content: "approval-done".into(),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_todos".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..assistant()
            });
        }

        // 8. "timeout approval" → call read_todos (will timeout)
        if user_text.contains("timeout approval") {
            if has_results {
                let outcome = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        RequestMessage::Tool(t) if t.name == "read_todos" => {
                            Some(format!("{:?}", t.outcome))
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                return completed(AssistantMessage {
                    content: format!("timeout-result: {outcome}"),
                    ..assistant()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_timeout".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..assistant()
            });
        }

        // Default fallback
        completed(AssistantMessage {
            content: format!("echo: {user_text}"),
            ..assistant()
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a minimal `AgentSpec` with only read_todos tool (no filesystem tools).
fn simple_spec(system_prompt: &str) -> AgentSpec {
    use coda_tools::ReadTodosToolSpec;

    AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: system_prompt.into(),
        mode: SubAgentMode::Stateful,
        tools: vec![Box::new(ReadTodosToolSpec)],
        subagents: vec![],
    }
}

/// A single-agent team (no sub-agents) for the `.team(...)` builder entry.
fn solo_team(spec: AgentSpec) -> AgentTeam {
    AgentTeam::new(spec, vec![]).expect("valid team")
}

fn fake_profile() -> ModelProfile<FakeProvider> {
    ModelProfile {
        provider: FakeProvider,
        model: "fake".into(),
        label: "fake".into(),
        temperature: None,
        max_completion_tokens: None,
        reasoning_effort: None,
        auto_compact_threshold_tokens: u32::MAX,
    }
}

fn run_config(approval: ToolApprovalMode) -> RunConfig<FakeProvider> {
    RunConfig {
        default_model: fake_profile(),
        agent_models: HashMap::new(),
        tool_approval: approval,
        approval_timeout: None,
    }
}

/// Collect events until the root agent produces a final `LLMEnd` with no tool
/// calls (i.e., the turn is complete). Returns the final assistant content.
/// Only considers events from the root agent — sub-agent events are ignored.
async fn collect_until_done(session: &Session) -> String {
    let deadline = Duration::from_secs(5);
    let result = timeout(deadline, async {
        loop {
            let Some(item) = session.recv().await else {
                panic!("session closed before turn completed");
            };
            let SessionStreamItem::Event(SessionEvent { origin, kind, .. }) = item else {
                continue;
            };
            if !origin.is_root() {
                continue;
            }
            match kind {
                AgentEvent::LLMEnd(msg) if msg.tool_calls.is_empty() => return msg.content,
                AgentEvent::Error(err) => panic!("root agent error: {err}"),
                AgentEvent::Aborted(target) => panic!("root agent aborted: {target:?}"),
                _ => {}
            }
        }
    })
    .await;
    result.expect("timed out waiting for turn to complete")
}

/// Collect events until the root agent produces a `Suspended` event for
/// approval. Returns the `PendingApproval`.
/// Only considers events from the root agent — sub-agent events are ignored.
async fn collect_until_suspended(session: &Session) -> coda_agent::PendingApproval {
    let deadline = Duration::from_secs(5);
    let result = timeout(deadline, async {
        loop {
            let Some(item) = session.recv().await else {
                panic!("session closed before suspension");
            };
            let SessionStreamItem::Event(SessionEvent { origin, kind, .. }) = item else {
                continue;
            };
            if !origin.is_root() {
                continue;
            }
            match kind {
                AgentEvent::Suspended(pending) => return pending,
                AgentEvent::Error(err) => panic!("root agent error: {err}"),
                AgentEvent::Aborted(target) => panic!("root agent aborted: {target:?}"),
                _ => {}
            }
        }
    })
    .await;
    result.expect("timed out waiting for suspension")
}

async fn collect_until_any_agent_suspends(session: &Session) -> coda_agent::PendingApproval {
    timeout(Duration::from_secs(5), async {
        loop {
            let Some(SessionStreamItem::Event(SessionEvent { kind, .. })) = session.recv().await
            else {
                continue;
            };
            if let AgentEvent::Suspended(pending) = kind {
                return pending;
            }
        }
    })
    .await
    .expect("timed out waiting for suspension")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Simple text reply through the Session API (no tools).
#[tokio::test]
async fn should_reply_with_text_when_no_tools_needed() {
    let spec = simple_spec("session-system");
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(spec), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    session
        .send(MessageId::new(), "simple hello", vec![])
        .await
        .expect("send");

    let reply = collect_until_done(&session).await;
    assert_eq!(reply, "Hello from the agent!");

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 2. Read a real file through the read_file tool.
#[tokio::test]
async fn should_read_file_via_tool_call() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file_path = tmp.path().join("hello.txt");
    std::fs::write(&file_path, "line one\nline two\n").expect("write temp file");

    let spec = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "session-system".into(),
        mode: SubAgentMode::Stateful,
        tools: coda_tools::builtin_specs(),
        subagents: vec![],
    };
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(spec), &tmp.path().to_string_lossy())
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    let task = format!("read file at {}", file_path.display());
    session
        .send(MessageId::new(), task, vec![])
        .await
        .expect("send");

    let reply = collect_until_done(&session).await;
    assert!(
        reply.contains("line one"),
        "expected file content in reply, got: {reply}"
    );

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 3. Write a file then read it back — validates multi-step tool chaining.
#[tokio::test]
async fn should_write_and_read_back_file() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let file_path = tmp.path().join("roundtrip.txt");

    let spec = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "session-system".into(),
        mode: SubAgentMode::Stateful,
        tools: coda_tools::builtin_specs(),
        subagents: vec![],
    };
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(spec), &tmp.path().to_string_lossy())
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    let task = format!("write then read {}", file_path.display());
    session
        .send(MessageId::new(), task, vec![])
        .await
        .expect("send");

    let reply = collect_until_done(&session).await;
    assert!(
        reply.contains("session-test-data"),
        "expected round-tripped content, got: {reply}"
    );

    let on_disk = std::fs::read_to_string(&file_path).expect("read from disk");
    assert_eq!(on_disk, "session-test-data");

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 4. Delegate a task to the explore sub-agent.
#[tokio::test]
async fn should_delegate_to_explore_subagent() {
    let coda = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "session-system".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec!["explore".into()],
    };
    let explore = AgentSpec {
        name: "explore".into(),
        description: "An explore sub-agent.".into(),
        system_prompt: "You are an exploration assistant.".into(),
        mode: SubAgentMode::Stateless,
        tools: vec![],
        subagents: vec![],
    };
    let team = AgentTeam::new(coda, vec![explore]).expect("valid team");

    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&team, ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    session
        .send(MessageId::new(), "delegate to explore", vec![])
        .await
        .expect("send");

    let reply = collect_until_done(&session).await;
    assert!(
        reply.contains("explore-done"),
        "expected explore result in reply, got: {reply}"
    );

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 5. Multi-turn conversation: send two tasks, verify both get responses.
#[tokio::test]
async fn should_maintain_history_across_turns() {
    let spec = simple_spec("session-system");
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(spec), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    // Turn 1
    session
        .send(MessageId::new(), "multi turn start", vec![])
        .await
        .expect("send turn 1");
    let reply1 = collect_until_done(&session).await;
    assert_eq!(reply1, "turn-1-reply");

    // Turn 2
    session
        .send(MessageId::new(), "multi turn follow", vec![])
        .await
        .expect("send turn 2");
    let reply2 = collect_until_done(&session).await;
    assert!(
        reply2.contains("saw 2 user messages"),
        "expected 2 user messages in history, got: {reply2}"
    );

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 6. Session resume: shutdown, re-open with same session_id, verify history.
#[tokio::test]
async fn should_resume_from_prior_checkpoint() {
    let storage = MemoryStorage::default();
    let session_id = "session-resume-test";

    // Session 1: send a task and get response
    let spec = simple_spec("session-system");
    let session1 = Session::builder()
        .storage(storage.clone())
        .team(&solo_team(spec), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .session_id(session_id)
        .open()
        .await
        .expect("open session 1");

    session1
        .send(MessageId::new(), "resume test start", vec![])
        .await
        .expect("send");
    let reply1 = collect_until_done(&session1).await;
    assert_eq!(reply1, "session-1-reply");

    session1
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    // Session 2: re-open with the same session_id and storage
    let spec2 = simple_spec("session-system");
    let session2 = Session::builder()
        .storage(storage.clone())
        .team(&solo_team(spec2), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .session_id(session_id)
        .open()
        .await
        .expect("open session 2");

    let resumed = session2
        .resumed_messages()
        .expect("expected resumed messages");
    let resumed_text: String = resumed
        .iter()
        .filter_map(|m| match m {
            Message::User(u) => u.first_text(),
            Message::Assistant(a) => Some(a.content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        resumed_text.contains("resume test start"),
        "resumed history should contain the first user message, got: {resumed_text}"
    );
    assert!(
        resumed_text.contains("session-1-reply"),
        "resumed history should contain the first assistant reply, got: {resumed_text}"
    );

    session2
        .send(MessageId::new(), "resume test follow", vec![])
        .await
        .expect("send");
    let reply2 = collect_until_done(&session2).await;
    assert!(
        reply2.contains("session-2-reply"),
        "expected session-2-reply, got: {reply2}"
    );
    assert!(
        reply2.contains("history-len:"),
        "expected history length in reply, got: {reply2}"
    );

    session2
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 7. Approval flow: suspend for approval, resume with Execute, turn completes.
#[tokio::test]
async fn should_execute_tool_after_approval_resume() {
    let spec = simple_spec("session-system");
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));

    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(spec), ".")
        .run_config(run_config(approval))
        .open()
        .await
        .expect("open session");

    session
        .send(MessageId::new(), "approve read_todos", vec![])
        .await
        .expect("send");

    let pending = collect_until_suspended(&session).await;
    assert_eq!(pending.calls.len(), 1);
    assert_eq!(pending.calls[0].name, "read_todos");

    session
        .resume(
            &pending.agent_name,
            &pending.thread_id,
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        )
        .await
        .expect("resume");

    let reply = collect_until_done(&session).await;
    assert_eq!(reply, "approval-done");

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// 8. Approval timeout: pending approval is auto-rejected when session reopens
///    after the configured timeout.
#[tokio::test]
async fn should_auto_reject_when_approval_times_out() {
    let storage = MemoryStorage::default();
    let session_id = "session-timeout-test";
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));

    // Session 1: trigger a tool that requires approval, don't resume, shutdown
    let spec = simple_spec("session-system");
    let session1 = Session::builder()
        .storage(storage.clone())
        .team(&solo_team(spec), ".")
        .run_config(run_config(approval.clone()))
        .session_id(session_id)
        .open()
        .await
        .expect("open session 1");

    session1
        .send(MessageId::new(), "timeout approval", vec![])
        .await
        .expect("send");

    let pending = collect_until_suspended(&session1).await;
    assert_eq!(pending.calls[0].name, "read_todos");

    session1
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    // Ensure the suspended_at timestamp is clearly in the past relative to
    // a zero timeout.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Session 2: re-open with approval_timeout = ZERO so the pending approval
    // is auto-rejected immediately.
    let spec2 = simple_spec("session-system");
    let session2 = Session::builder()
        .storage(storage.clone())
        .team(&solo_team(spec2), ".")
        .run_config(RunConfig {
            approval_timeout: Some(Duration::ZERO),
            ..run_config(approval)
        })
        .session_id(session_id)
        .open()
        .await
        .expect("open session 2 (should auto-reject)");

    let reply = collect_until_done(&session2).await;
    assert!(
        reply.contains("Rejected"),
        "expected rejection outcome in reply, got: {reply}"
    );

    session2
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

#[tokio::test]
async fn should_find_a_subagent_approval_without_a_runtime_snapshot() {
    use coda_tools::ReadTodosToolSpec;

    let source = MemoryStorage::default();
    let cold = MemoryStorage::default();
    let session_id = "session-subagent-approval-without-snapshot";
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "session-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: "An explore sub-agent.".into(),
            system_prompt: "You are an exploration assistant.".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        }],
    )
    .expect("valid team");

    let session = Session::builder()
        .storage(source.clone())
        .team(&team, ".")
        .run_config(run_config(approval.clone()))
        .session_id(session_id)
        .open()
        .await
        .expect("open source session");
    session
        .send(MessageId::new(), "delegate approval to explore", vec![])
        .await
        .expect("send");

    let pending = collect_until_any_agent_suspends(&session).await;
    assert_eq!(pending.agent_name, "explore");
    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    // A process killed before its first snapshot leaves this durable shape; so
    // does a fresh fork after it starts work and reaches this suspension.
    for checkpoint in source.all_checkpoints().await {
        cold.save_checkpoint(checkpoint.thread_id.clone(), checkpoint)
            .await
            .expect("copy checkpoint");
    }
    assert!(
        cold.load_session_snapshot(session_id)
            .await
            .expect("load snapshot")
            .is_none()
    );

    let discovered = match Session::builder()
        .storage(cold.clone())
        .team(&team, ".")
        .run_config(run_config(approval.clone()))
        .session_id(session_id)
        .open()
        .await
    {
        Err(OpenError::PendingApprovalsRequired(pending)) => pending,
        Err(err) => panic!("unexpected open error: {err}"),
        Ok(session) => {
            session
                .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
                .await;
            panic!("sub-agent approval was not discovered");
        }
    };
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].thread_id, pending.thread_id);

    let resumed = Session::builder()
        .storage(cold)
        .team(&team, ".")
        .run_config(run_config(approval))
        .session_id(session_id)
        .resume_decisions(HashMap::from([(
            pending.thread_id.clone(),
            ResumeDecision {
                parent_message_id: pending.parent_message_id,
                resolutions: vec![(pending.calls[0].id.clone(), ToolCallResolution::Execute)],
            },
        )]))
        .open()
        .await
        .expect("resume sub-agent approval without a snapshot");
    assert_eq!(collect_until_done(&resumed).await, "subagent-approval-done");
    resumed
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

#[tokio::test]
async fn should_ignore_a_removed_agents_approval_across_reopens() {
    use coda_tools::ReadTodosToolSpec;

    let storage = MemoryStorage::default();
    let session_id = "session-removed-agent-approval";
    let approval = ToolApprovalMode::RequireWhen(Arc::new(|call| call.name == "read_todos"));
    let original_team = AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "session-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: "An explore sub-agent.".into(),
            system_prompt: "You are an exploration assistant.".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![Box::new(ReadTodosToolSpec)],
            subagents: vec![],
        }],
    )
    .expect("valid original team");

    let original = Session::builder()
        .storage(storage.clone())
        .team(&original_team, ".")
        .run_config(run_config(approval.clone()))
        .session_id(session_id)
        .open()
        .await
        .expect("open original session");
    original
        .send(MessageId::new(), "delegate approval to explore", vec![])
        .await
        .expect("send");
    let pending = collect_until_any_agent_suspends(&original).await;
    assert_eq!(pending.agent_name, "explore");
    original
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    let current_team = solo_team(simple_spec("session-system"));
    let reopened = Session::builder()
        .storage(storage.clone())
        .team(&current_team, ".")
        .run_config(run_config(approval.clone()))
        .session_id(session_id)
        .open()
        .await
        .expect("removed agent must not require an approval decision");
    reopened
        .send(MessageId::new(), "simple hello", vec![])
        .await
        .expect("the removed agent's stale turn must not block new work");
    assert_eq!(collect_until_done(&reopened).await, "Hello from the agent!");
    reopened
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    let still_pending = storage
        .load_pending_approval_checkpoints(session_id)
        .await
        .expect("load pending approvals");
    assert!(
        still_pending
            .iter()
            .any(|checkpoint| checkpoint.agent_name == "explore"),
        "ignoring an unavailable agent must not destroy its checkpoint"
    );

    let reopened_again = Session::builder()
        .storage(storage)
        .team(&current_team, ".")
        .run_config(run_config(approval))
        .session_id(session_id)
        .open()
        .await
        .expect("the same checkpoint must not block a later reopen");
    reopened_again
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// State a tool records must reach the checkpoint on the message that recorded
/// it, and come back on the next turn. This is the whole path —
/// `ThreadState::set` → the call's result → `HistoryEntry::state` → storage →
/// restore → `ThreadState::get` — which the unit tests either side of it
/// cannot cover.
#[tokio::test]
async fn tool_state_survives_the_turn_that_recorded_it() {
    let spec = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "session-system".into(),
        mode: SubAgentMode::Stateful,
        tools: coda_tools::builtin_specs(),
        subagents: vec![],
    };
    let storage = MemoryStorage::default();
    let session = Session::builder()
        .storage(storage.clone())
        .session_id("todo-session")
        .team(&solo_team(spec), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    session
        .send(MessageId::new(), "plan the work", vec![])
        .await
        .expect("send");
    assert_eq!(collect_until_done(&session).await, "planned");

    // On the message, not beside it: the entry carrying the write is the tool
    // result that made it.
    let checkpoint = storage
        .load_checkpoint("todo-session")
        .await
        .expect("load")
        .expect("a checkpoint");
    let recorded = checkpoint
        .messages
        .iter()
        .find(|entry| entry.state.contains_key("todos"))
        .expect("the write was recorded");
    assert!(
        matches!(&recorded.message, Message::Tool(_)),
        "the write must ride on the tool result that made it, or no fork or \
         rewind can reach the state through it"
    );

    // And a later turn reads it back through the context.
    session
        .send(MessageId::new(), "what is left", vec![])
        .await
        .expect("send");
    assert_eq!(
        collect_until_done(&session).await,
        "todos: 1. [x] parse\n2. [ ] test\n"
    );

    session
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;
}

/// A session that built its own task registry owns it: shutting the session
/// down kills the background work it started, rather than leaving orphans
/// behind a registry nobody can reach any more.
#[tokio::test]
async fn should_kill_owned_background_tasks_on_shutdown() {
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .team(&solo_team(simple_spec("session-system")), ".")
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    let pidfile = std::env::temp_dir().join(format!("coda-session-bg-{}", std::process::id()));
    let _ = std::fs::remove_file(&pidfile);
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(format!("echo $$ > '{}'; sleep 43.11", pidfile.display()));
    session
        .background()
        .spawn(
            cmd,
            coda_process::TaskMeta {
                command: "sleep".into(),
                description: "owned task".into(),
                agent_name: "coda".into(),
            },
        )
        .await
        .expect("spawn background task");

    let pid: i32 = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("task never reported its pid");

    assert!(
        session
            .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
            .await,
        "session did not confirm exit, so the registry is deliberately left alone"
    );

    // SAFETY: signal 0 only probes for existence.
    timeout(Duration::from_secs(5), async {
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background process survived session shutdown");

    let _ = std::fs::remove_file(&pidfile);
}
