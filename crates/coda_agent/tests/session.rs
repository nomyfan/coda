//! Session lifecycle tests.
//!
//! Exercise the full `Session` API (builder → open → send → recv → shutdown)
//! with a fake LLM provider, covering real built-in tools, multi-turn
//! conversations, sub-agent delegation, session resume, and approval flows.

use std::sync::Arc;
use std::time::Duration;

use coda_agent::runtime::MemoryStorage;
use coda_agent::{
    AgentEvent, AgentSpec, ResumeDecision, RunConfig, Session, SessionEvent, Shutdown,
    SubAgentMode, ToolApprovalMode, ToolCallResolution,
};
use coda_core::llm::{
    AssistantMessage, ChatCompletionRequest, LLMStreamEvent, Message, StreamError, ToolCall,
    ToolOutput,
};
use coda_tools::BuildContext;
use futures::{Stream, stream};
use serde_json::json;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// FakeProvider — a mock LLMProvider that routes based on message content
// ---------------------------------------------------------------------------

/// Extracts the text of the last `User` message in a request.
fn last_user_text(messages: &[Message]) -> &str {
    messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::User(u) => Some(u.0.as_str()),
            _ => None,
        })
        .unwrap_or("")
}

/// Returns `true` if the message list contains any `Tool` message.
fn has_tool_results(messages: &[Message]) -> bool {
    messages.iter().any(|m| matches!(m, Message::Tool(_)))
}

/// Count the number of `User` messages in the request.
fn user_message_count(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, Message::User(_)))
        .count()
}

fn completed(
    msg: AssistantMessage,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<LLMStreamEvent, StreamError>> + Send>> {
    Box::pin(stream::iter(vec![Ok(LLMStreamEvent::Completed(msg))]))
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
                ..Default::default()
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
                        Message::Tool(t) if t.name == "read_file" => {
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
                    ..Default::default()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_read".into(),
                    name: "read_file".into(),
                    arguments: Some(json!({"file_path": path}).to_string()),
                }],
                ..Default::default()
            });
        }

        // 3. "write then read <path>" → write_file first, then read_file
        if let Some(path) = user_text.strip_prefix("write then read ") {
            let path = path.trim();

            let has_write = request
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool(t) if t.name == "write_file"));
            let has_read = request
                .messages
                .iter()
                .any(|m| matches!(m, Message::Tool(t) if t.name == "read_file"));

            if has_read {
                let read_output = request
                    .messages
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::Tool(t) if t.name == "read_file" => {
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
                    ..Default::default()
                });
            } else if has_write {
                return completed(AssistantMessage {
                    tool_calls: vec![ToolCall {
                        id: "call_read".into(),
                        name: "read_file".into(),
                        arguments: Some(json!({"file_path": path}).to_string()),
                    }],
                    ..Default::default()
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
                    ..Default::default()
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
                        Message::Tool(t) if t.name == "explore" => {
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
                    ..Default::default()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_explore".into(),
                    name: "explore".into(),
                    arguments: Some(r#"{"task":"session probe"}"#.into()),
                }],
                ..Default::default()
            });
        }

        // For explore sub-agent: respond with a simple message
        if user_text.contains("session probe") {
            return completed(AssistantMessage {
                content: "explore-done".into(),
                ..Default::default()
            });
        }

        // 5. Multi-turn: "multi turn start" then "multi turn follow"
        if user_text.contains("multi turn start") {
            return completed(AssistantMessage {
                content: "turn-1-reply".into(),
                ..Default::default()
            });
        }
        if user_text.contains("multi turn follow") {
            let count = user_message_count(&request.messages);
            return completed(AssistantMessage {
                content: format!("turn-2-reply (saw {count} user messages)"),
                ..Default::default()
            });
        }

        // 6. "resume test start" / "resume test follow"
        if user_text.contains("resume test start") {
            return completed(AssistantMessage {
                content: "session-1-reply".into(),
                ..Default::default()
            });
        }
        if user_text.contains("resume test follow") {
            let total = request.messages.len();
            return completed(AssistantMessage {
                content: format!("session-2-reply (history-len: {total})"),
                ..Default::default()
            });
        }

        // 7. "approve read_todos" → call read_todos (requires approval)
        if user_text.contains("approve read_todos") {
            if has_results {
                return completed(AssistantMessage {
                    content: "approval-done".into(),
                    ..Default::default()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_todos".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..Default::default()
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
                        Message::Tool(t) if t.name == "read_todos" => {
                            Some(format!("{:?}", t.outcome))
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                return completed(AssistantMessage {
                    content: format!("timeout-result: {outcome}"),
                    ..Default::default()
                });
            }
            return completed(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "call_timeout".into(),
                    name: "read_todos".into(),
                    arguments: Some("{}".into()),
                }],
                ..Default::default()
            });
        }

        // Default fallback
        completed(AssistantMessage {
            content: format!("echo: {user_text}"),
            ..Default::default()
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

fn run_config(approval: ToolApprovalMode) -> RunConfig<FakeProvider> {
    RunConfig {
        provider: FakeProvider,
        model: "fake".into(),
        temperature: None,
        max_completion_tokens: None,
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
            let Some(SessionEvent { origin, kind, .. }) = session.recv().await else {
                panic!("session closed before turn completed");
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
            let Some(SessionEvent { origin, kind, .. }) = session.recv().await else {
                panic!("session closed before suspension");
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// 1. Simple text reply through the Session API (no tools).
#[tokio::test]
async fn should_reply_with_text_when_no_tools_needed() {
    let spec = simple_spec("session-system");
    let session = Session::builder()
        .storage(MemoryStorage::default())
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    session.send("simple hello").await.expect("send");

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
    let ctx = BuildContext::new(tmp.path().to_string_lossy());

    let session = Session::builder()
        .storage(MemoryStorage::default())
        .root(spec)
        .build_context(ctx)
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    let task = format!("read file at {}", file_path.display());
    session.send(task).await.expect("send");

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
    let ctx = BuildContext::new(tmp.path().to_string_lossy());

    let session = Session::builder()
        .storage(MemoryStorage::default())
        .root(spec)
        .build_context(ctx)
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    let task = format!("write then read {}", file_path.display());
    session.send(task).await.expect("send");

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
    let spec = AgentSpec {
        name: "coda".into(),
        description: String::new(),
        system_prompt: "session-system".into(),
        mode: SubAgentMode::Stateful,
        tools: vec![],
        subagents: vec![AgentSpec {
            name: "explore".into(),
            description: "An explore sub-agent.".into(),
            system_prompt: "You are an exploration assistant.".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
    };

    let session = Session::builder()
        .storage(MemoryStorage::default())
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    session.send("delegate to explore").await.expect("send");

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
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(run_config(ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    // Turn 1
    session.send("multi turn start").await.expect("send turn 1");
    let reply1 = collect_until_done(&session).await;
    assert_eq!(reply1, "turn-1-reply");

    // Turn 2
    session
        .send("multi turn follow")
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
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(run_config(ToolApprovalMode::Auto))
        .session_id(session_id)
        .open()
        .await
        .expect("open session 1");

    session1.send("resume test start").await.expect("send");
    let reply1 = collect_until_done(&session1).await;
    assert_eq!(reply1, "session-1-reply");

    session1
        .shutdown(Shutdown::graceful_then_abort(Duration::from_secs(2)))
        .await;

    // Session 2: re-open with the same session_id and storage
    let spec2 = simple_spec("session-system");
    let session2 = Session::builder()
        .storage(storage.clone())
        .root(spec2)
        .build_context(BuildContext::new("."))
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
            Message::User(u) => Some(u.0.as_str()),
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

    session2.send("resume test follow").await.expect("send");
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
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(RunConfig {
            provider: FakeProvider,
            model: "fake".into(),
            temperature: None,
            max_completion_tokens: None,
            tool_approval: approval,
            approval_timeout: None,
        })
        .open()
        .await
        .expect("open session");

    session.send("approve read_todos").await.expect("send");

    let pending = collect_until_suspended(&session).await;
    assert_eq!(pending.calls.len(), 1);
    assert_eq!(pending.calls[0].name, "read_todos");

    session
        .resume(
            &pending.agent_name,
            &pending.thread_id,
            ResumeDecision {
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
        .root(spec)
        .build_context(BuildContext::new("."))
        .run_config(RunConfig {
            provider: FakeProvider,
            model: "fake".into(),
            temperature: None,
            max_completion_tokens: None,
            tool_approval: approval.clone(),
            approval_timeout: None,
        })
        .session_id(session_id)
        .open()
        .await
        .expect("open session 1");

    session1.send("timeout approval").await.expect("send");

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
        .root(spec2)
        .build_context(BuildContext::new("."))
        .run_config(RunConfig {
            provider: FakeProvider,
            model: "fake".into(),
            temperature: None,
            max_completion_tokens: None,
            tool_approval: approval,
            approval_timeout: Some(Duration::ZERO),
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
