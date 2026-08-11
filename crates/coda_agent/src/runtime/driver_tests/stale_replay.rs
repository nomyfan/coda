//! Recovery from a snapshot that still holds envelopes an earlier recovery
//! already delivered. Nothing rewrites the snapshot until an agent exits, so a
//! second crash replays the same ones.

use super::super::*;
use super::fixtures::*;
use crate::{
    AgentEvent, AgentSpec, AgentTeam, ResumeDecision, SubAgentMode, ToolApprovalMode,
    agent::HistoryEntry,
    persist::{StoredResumePoint, StoredToolExecutionState},
    runtime::{AgentRuntimeSnapshot, MemoryStorage, SendCommandError, SessionStorage},
};
use coda_core::llm::{Message, MessageId, ToolCall, ToolCallOutcome, ToolOutput, UserMessage};
use std::collections::HashMap;
use tokio::sync::broadcast;
use tokio::time::{Duration, timeout};

const SESSION: &str = "session";
const EXPLORE_THREAD: &str = "explore-thread";
/// Stands in for the id of the envelope that carried a sub-agent call out.
const DISPATCH: &str = "dispatch-envelope";

type Events = broadcast::Receiver<(String, ThreadId, TurnId, AgentEvent)>;

fn team() -> AgentTeam {
    AgentTeam::new(
        AgentSpec {
            name: "coda".into(),
            description: String::new(),
            system_prompt: "main-system".into(),
            mode: SubAgentMode::Stateful,
            tools: vec![],
            subagents: vec!["explore".into()],
        },
        vec![AgentSpec {
            name: "explore".into(),
            description: String::new(),
            system_prompt: "explore-plain".into(),
            mode: SubAgentMode::Stateless,
            tools: vec![],
            subagents: vec![],
        }],
    )
    .expect("valid team")
}

/// The root's history at the point it dispatched `call_explore`: a turn is
/// open, and its last message is the assistant message that made the call.
fn dispatched(parent_message_id: MessageId) -> (TurnId, Vec<HistoryEntry>) {
    let prompt = MessageId::new();
    let turn = TurnId::from(prompt);
    let messages = vec![
        HistoryEntry {
            turn_id: turn,
            message: Message::User(UserMessage::text(prompt, "inspect")),
        },
        HistoryEntry {
            turn_id: turn,
            message: Message::Assistant(AssistantMessage {
                message_id: parent_message_id,
                tool_calls: vec![ToolCall {
                    id: "call_explore".into(),
                    name: "explore".into(),
                    arguments: Some(r#"{"task":"inspect the crate"}"#.into()),
                }],
                ..assistant()
            }),
        },
    ];
    (turn, messages)
}

async fn store_root(
    storage: &impl SessionStorage,
    messages: Vec<HistoryEntry>,
    at: StoredResumePoint,
) {
    storage
        .save_checkpoint(
            SESSION.to_string(),
            StoredCheckpoint {
                thread_id: SESSION.to_string(),
                agent_name: "coda".into(),
                parent_thread_id: None,
                derivation_key: None,
                reply_target: None,
                messages,
                todos: vec![],
                resume_point: at,
                suspended_at: jiff::Timestamp::default(),
            },
        )
        .await
        .expect("save checkpoint");
}

/// The answer to the call `dispatched_by` carried out.
fn explore_reply(dispatched_by: &str) -> Envelope {
    Envelope::with_id(|id| Envelope {
        id,
        from: Sender::Agent {
            name: "explore".into(),
            thread_id: ThreadId::from(EXPLORE_THREAD.to_string()),
        },
        to: Receiver {
            name: "coda".into(),
            thread_id: ThreadId::from(SESSION.to_string()),
        },
        reply_to: Some(dispatched_by.to_string()),
        body: EnvelopeBody::Reply {
            call_id: "call_explore".into(),
            output: ToolOutput::Ok("explore done".into()),
            aborted: false,
        },
    })
}

/// The root parked on the answer to the call `dispatched_by` carried out.
fn awaiting(parent_message_id: MessageId, dispatched_by: &str) -> StoredResumePoint {
    StoredResumePoint::ToolExecution(StoredToolExecutionState {
        parent_message_id,
        pending_replies: vec![PendingReply {
            call_id: "call_explore".into(),
            call_envelope_id: dispatched_by.to_string(),
            tool_name: "explore".into(),
            outcome: ToolCallOutcome::Approved,
            started_at: jiff::Timestamp::default(),
        }],
        tool_calls: vec![],
    })
}

fn snapshot_holding(envelope: Envelope) -> AgentRuntimeSnapshot {
    AgentRuntimeSnapshot {
        agent_drained_envelopes: HashMap::from([("coda".to_string(), vec![envelope])]),
        ..Default::default()
    }
}

/// Subscribes before the agents start, so nothing they emit on the way up is
/// missed.
async fn bootstrapped(
    storage: impl SessionStorage + Clone + 'static,
    snapshot: AgentRuntimeSnapshot,
) -> (AgentRuntime, Events) {
    let mut runtime = AgentRuntime::new(storage, SESSION.to_string());
    let events = runtime.subscribe();
    runtime
        .bootstrap(
            team().build(".", coda_tools::shared_file_locks()),
            Some(snapshot),
            HashMap::new(),
            test_config(TestProvider::default(), ToolApprovalMode::Auto),
        )
        .await
        .expect("bootstrap");
    (runtime, events)
}

async fn root_answers(events: &mut Events) {
    timeout(Duration::from_secs(2), async {
        loop {
            let (agent_name, _, _, event) = events.recv().await.expect("receive event");
            if let ("coda", AgentEvent::LLMEnd(message)) = (agent_name.as_str(), event)
                && message.content == "main done"
            {
                return;
            }
        }
    })
    .await
    .expect("the root never answered");
}

fn root_task() -> Envelope {
    user_task(&ThreadId::from(SESSION.to_string()), "carry on")
}

/// The turn this reply belonged to is over: the root took the answer and
/// finished, and its checkpoint still carries that turn's last message.
/// Restoring the turn from the reply would open the gate on work nothing is
/// coming to end.
#[tokio::test]
async fn a_reply_the_root_already_took_opens_no_turn() {
    let storage = MemoryStorage::default();
    let (_, messages) = dispatched(MessageId::new());
    store_root(&storage, messages, StoredResumePoint::Generation).await;

    let (runtime, _events) = bootstrapped(storage, snapshot_holding(explore_reply(DISPATCH))).await;

    assert_eq!(runtime.turn_gate.active_id(), None);
    assert!(
        !runtime
            .calls
            .is_answering(&ThreadId::from(EXPLORE_THREAD.to_string())),
        "the answer was already taken, so nothing is owed for it"
    );
    assert!(
        runtime.send_message(root_task()).await.is_ok(),
        "the session refused a new task"
    );
}

/// The counterpart: a reply the root is still parked on is live evidence, and
/// has to survive the replay — both the turn it belongs to and the envelope
/// that carries it. Checkpoint writes are held so the turn cannot finish before
/// the gate is read.
#[tokio::test]
async fn a_reply_the_root_is_still_waiting_for_survives_the_replay() {
    let storage = TestStorage::default();
    let parent_message_id = MessageId::new();
    let (turn, messages) = dispatched(parent_message_id);
    store_root(&storage, messages, awaiting(parent_message_id, DISPATCH)).await;
    let held = storage.hold_checkpoints_of("coda").await;

    let (runtime, mut events) =
        bootstrapped(storage, snapshot_holding(explore_reply(DISPATCH))).await;

    assert_eq!(runtime.turn_gate.active_id(), Some(turn));

    held.release().await;
    root_answers(&mut events).await;
    assert_eq!(runtime.turn_gate.active_id(), None);
}

/// The root really is parked on a call, and that call really does carry the
/// `call_id` this answer names — but it is a later invocation that reused the
/// id. Matching on the id alone would hand it the previous call's result.
#[tokio::test]
async fn a_reply_to_an_earlier_call_that_reused_the_id_is_not_its_answer() {
    let storage = MemoryStorage::default();
    let parent_message_id = MessageId::new();
    let (_, messages) = dispatched(parent_message_id);
    store_root(
        &storage,
        messages,
        awaiting(parent_message_id, "a-later-dispatch"),
    )
    .await;

    let (runtime, _events) = bootstrapped(storage, snapshot_holding(explore_reply(DISPATCH))).await;

    assert!(
        !runtime
            .calls
            .is_answering(&ThreadId::from(EXPLORE_THREAD.to_string())),
        "an answer from a previous invocation was taken for the current one"
    );
}

/// The public path the turn gate cannot speak for. A snapshot holding nothing
/// but a stale answer resumes nothing, and a caller told otherwise would show
/// the session busy for a turn that ended in another process — forever, since
/// no settlement is coming.
#[tokio::test]
async fn a_session_resuming_nothing_does_not_report_resuming_agents() {
    let storage = MemoryStorage::default();
    let (_, messages) = dispatched(MessageId::new());
    store_root(&storage, messages, StoredResumePoint::Generation).await;
    storage
        .save_session_snapshot(
            SESSION.to_string(),
            snapshot_holding(explore_reply(DISPATCH)).into(),
        )
        .await
        .expect("save snapshot");

    let session = crate::SessionBuilder::new()
        .storage(storage)
        .team(&team(), ".")
        .session_id(SESSION)
        .run_config(test_config(TestProvider::default(), ToolApprovalMode::Auto))
        .open()
        .await
        .expect("open session");

    assert!(
        !session.has_resuming_agents(),
        "the session reported work it had already thrown away"
    );
}

/// A checkpoint that cannot be read says nothing about whether the answer it
/// would judge is still owed. Refusing to open leaves that answer on disk for
/// the next attempt; guessing either way would not.
#[tokio::test]
async fn a_checkpoint_that_cannot_be_read_refuses_the_recovery() {
    let storage = TestStorage::default();
    let (_, messages) = dispatched(MessageId::new());
    store_root(&storage, messages, StoredResumePoint::Generation).await;
    storage.fail_checkpoint_loads().await;

    let mut runtime = AgentRuntime::new(storage, SESSION.to_string());
    let error = runtime
        .bootstrap(
            team().build(".", coda_tools::shared_file_locks()),
            Some(snapshot_holding(explore_reply(DISPATCH))),
            HashMap::new(),
            test_config(TestProvider::default(), ToolApprovalMode::Auto),
        )
        .await
        .expect_err("recovery guessed at a checkpoint it could not read");

    assert!(error.contains("failed to load checkpoint"), "{error}");
}

/// Same rule for the other envelope that re-opens no work of its own: a
/// decision the thread it was meant for has already acted on.
#[tokio::test]
async fn a_resume_for_a_thread_that_is_no_longer_parked_opens_no_turn() {
    let storage = MemoryStorage::default();
    let (_, messages) = dispatched(MessageId::new());
    store_root(&storage, messages, StoredResumePoint::Generation).await;
    let resume = Envelope::with_id(|id| Envelope {
        id,
        from: Sender::User,
        to: Receiver {
            name: "coda".into(),
            thread_id: ThreadId::from(SESSION.to_string()),
        },
        reply_to: None,
        body: EnvelopeBody::Resume(ResumeDecision {
            resolutions: vec![],
        }),
    });

    let (runtime, _events) = bootstrapped(storage, snapshot_holding(resume)).await;

    assert_eq!(runtime.turn_gate.active_id(), None);
    assert!(
        !matches!(
            runtime.send_message(root_task()).await,
            Err(SendCommandError::TurnAlreadyActive)
        ),
        "the session refused a new task"
    );
}
