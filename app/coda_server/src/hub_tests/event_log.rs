//! Pure logic tests for `EventLog`, `fold_settled_turn`, and
//! `event_settles_turn` — no hub or session involved.

use super::super::*;
use super::fixtures::assistant;
use coda_core::llm::{AssistantMessage, ToolMessage, ToolOutput};

fn tool_message(id: &str, text: &str) -> ToolMessage {
    ToolMessage::new(
        id.to_string(),
        "echo".to_string(),
        ToolOutput::Ok(text.to_string()),
        coda_core::llm::ToolCallOutcome::Auto,
        None,
    )
}

fn llm_end(agent: &str, message: AssistantMessage) -> WireEvent {
    WireEvent::LlmEnd {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message,
    }
}

fn tool_end(agent: &str, message: ToolMessage) -> WireEvent {
    WireEvent::ToolCallEnd {
        agent_name: agent.into(),
        thread_id: "t".into(),
        message,
    }
}

fn chunk(agent: &str, text: &str) -> WireEvent {
    WireEvent::LlmContentChunk {
        agent_name: agent.into(),
        thread_id: "t".into(),
        content: text.into(),
    }
}

fn user(text: &str) -> Message {
    Message::User(UserMessage::text(MessageId::new(), text.to_string()))
}

// --- EventLog ----------------------------------------------------------

#[test]
fn event_log_overflow_drops_oldest_chunk_tier_first() {
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..limits.max_log_events {
        if i == 10 {
            log.push(tool_end("coda", tool_message("keep", "kept")));
        } else {
            log.push(chunk("coda", &format!("c{i}")));
        }
    }
    log.push(llm_end("coda", assistant("fin")));
    assert_eq!(log.entries.len(), limits.max_log_events);
    // The oldest chunk was evicted; the message-tier events survive.
    assert!(matches!(
        log.entries.front(),
        Some(WireEvent::LlmContentChunk { content, .. }) if content == "c1"
    ));
    assert!(
        log.iter()
            .any(|e| matches!(e, WireEvent::ToolCallEnd { message, .. } if message.id == "keep"))
    );
}

#[test]
fn event_log_all_message_tier_grows_past_chunk_cap() {
    // `push` itself never drops a message-tier entry — dropping one would
    // corrupt the fold. Bounding this case is `message_tier_overflowed`'s
    // job (checked below), enforced by the forwarder forcing a resync;
    // see `runaway_tool_calls_force_resync_instead_of_unbounded_log`.
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..(limits.max_log_events + 5) {
        log.push(tool_end("coda", tool_message(&format!("m{i}"), "x")));
    }
    assert_eq!(log.entries.len(), limits.max_log_events + 5);
}

#[test]
fn event_log_message_tier_overflow_flag() {
    let limits = RelayConfig::default();
    let mut log = EventLog::new(limits);
    for i in 0..limits.max_message_tier_events {
        log.push(tool_end("coda", tool_message(&format!("m{i}"), "x")));
        assert!(!log.message_tier_overflowed());
    }
    log.push(tool_end("coda", tool_message("one_too_many", "x")));
    assert!(log.message_tier_overflowed());

    // Settling (which folds and clears the log) resets the count.
    log.clear();
    assert!(!log.message_tier_overflowed());
}

// --- fold_settled_turn ---------------------------------------------------

#[test]
fn fold_orders_stale_cleanup_before_user() {
    // History order on a stale-envelope turn: aborted ToolMessages first,
    // then the new user prompt, then the assistant reply.
    let mut snapshot = vec![];
    let turn = TurnId::from(MessageId::new());
    let mut users = vec![(turn, user("new task"))];
    let mut log = EventLog::new(RelayConfig::default());
    log.push(tool_end("coda", tool_message("stale1", "aborted")));
    log.push(tool_end("coda", tool_message("stale2", "aborted")));
    log.push(chunk("coda", "hi"));
    log.push(llm_end("coda", assistant("reply")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda", turn);

    assert_eq!(snapshot.len(), 4);
    assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "stale1"));
    assert!(matches!(&snapshot[1], Message::Tool(t) if t.id == "stale2"));
    assert!(matches!(&snapshot[2], Message::User(_)));
    assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "reply"));
    assert!(log.entries.is_empty());
    assert!(users.is_empty());
}

#[test]
fn fold_skips_subagent_and_chunk_events() {
    let mut snapshot = vec![];
    let turn = TurnId::from(MessageId::new());
    let mut users = vec![(turn, user("task"))];
    let mut log = EventLog::new(RelayConfig::default());
    log.push(chunk("coda", "x"));
    log.push(llm_end("coda", assistant("delegating")));
    log.push(llm_end("explore", assistant("sub result")));
    log.push(tool_end("explore", tool_message("sub_call", "sub")));
    log.push(tool_end(
        "coda",
        tool_message("agent_call", "reply from sub"),
    ));
    log.push(llm_end("coda", assistant("done")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda", turn);

    // user, assistant(delegating), tool(agent_call), assistant(done)
    assert_eq!(snapshot.len(), 4);
    assert!(matches!(&snapshot[0], Message::User(_)));
    assert!(matches!(&snapshot[1], Message::Assistant(a) if a.content == "delegating"));
    assert!(matches!(&snapshot[2], Message::Tool(t) if t.id == "agent_call"));
    assert!(matches!(&snapshot[3], Message::Assistant(a) if a.content == "done"));
}

#[test]
fn fold_tolerates_missing_user_for_resumed_turns() {
    let turn = TurnId::from(MessageId::new());
    let mut snapshot = vec![];
    let mut users = Vec::new();
    let mut log = EventLog::new(RelayConfig::default());
    log.push(tool_end("coda", tool_message("resolved", "ok")));
    log.push(llm_end("coda", assistant("after resume")));

    fold_settled_turn(&mut snapshot, &mut users, &mut log, "coda", turn);

    assert_eq!(snapshot.len(), 2);
    assert!(matches!(&snapshot[0], Message::Tool(t) if t.id == "resolved"));
    assert!(matches!(&snapshot[1], Message::Assistant(_)));
}

// --- event_settles_turn --------------------------------------------------

#[test]
fn settle_ignores_aborted_llm_end() {
    let mut aborted = assistant("partial");
    aborted.aborted = true;
    assert!(!event_settles_turn(&llm_end("coda", aborted), "coda"));
    assert!(event_settles_turn(
        &llm_end("coda", assistant("done")),
        "coda"
    ));
    assert!(!event_settles_turn(
        &llm_end("explore", assistant("sub")),
        "coda"
    ));
    assert!(event_settles_turn(
        &WireEvent::Aborted {
            agent_name: "coda".into(),
            thread_id: "t".into(),
            target: crate::wire::AbortedTargetWire::Generation,
        },
        "coda"
    ));
}
