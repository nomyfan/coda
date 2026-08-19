//! What a compaction may summarize, and the messages its answer becomes.
//!
//! The conversation is flattened to plain text rather than replayed as a
//! conversation. That drops three provider-compatibility problems at once — the
//! request would otherwise have to carry the full tool definitions behind every
//! `tool_calls` in the history, preserve truncated reasoning continuations, and
//! guarantee no tool result is left without its call. None of it earns its keep
//! for what is really a single instruction: summarize this record.
//!
//! [`cutoff`] is the one rule both a manual (idle) and an automatic (mid-turn)
//! compaction call to decide what's new since the last compaction — the manual
//! caller passes `protect: None` (the whole history is fair game), the
//! automatic one passes the in-progress turn. Neither caller re-derives the
//! answer, so they can never disagree about a boundary.

use crate::agent::HistoryEntry;
use crate::message_view::{self, COMPACTION_FAILED_KIND, COMPACTION_KIND};
use coda_core::llm::{
    ChatCompletionRequest, ContentPart, CustomMessage, CustomRole, Message, MessageId,
    RequestMessage, SystemMessage, ToolOutput, TurnId, UserMessage,
};

static COMPACTION_PROMPT: &str = include_str!("compaction-prompt.md");

/// The longest `instructions` a client may send. Generous for a line typed into
/// a composer, and small enough that it cannot crowd out the transcript.
pub const MAX_INSTRUCTIONS: usize = 4096;

/// What a compaction may summarize right now, or `None` when there is nothing
/// new to summarize since the last one.
///
/// `protect` names the turn whose own messages must stay out of the summary —
/// the in-progress turn, for a mid-turn/automatic compaction — or `None` when
/// nothing needs protecting (the idle/manual case, where the whole history is
/// fair game).
///
/// "Nothing new" covers three shapes: an empty thread; a protected turn with
/// no predecessor turn at all (the thread's very first turn); and a target
/// that does not fall after the last successful compaction. That last one is
/// also why a turn can compact at most once: `protect` pins the target to the
/// same message for as long as the turn is current, so once a compaction
/// succeeds past it, nothing later in the same turn re-qualifies. A *failed*
/// attempt does not move that boundary — it isn't recorded as [`COMPACTION_KIND`]
/// — so a later detection point in the same turn will try again.
pub fn cutoff(messages: &[HistoryEntry], protect: Option<TurnId>) -> Option<MessageId> {
    let target_idx = match protect {
        None => messages.len().checked_sub(1)?,
        Some(turn) => messages.iter().rposition(|entry| entry.turn_id != turn)?,
    };
    let boundary_idx = messages
        .iter()
        .rposition(|entry| message_view::is_compaction_summary(&entry.message));
    if boundary_idx.is_some_and(|boundary_idx| target_idx <= boundary_idx) {
        return None;
    }
    Some(messages[target_idx].message.message_id())
}

/// What prompted a compaction, and how the summary should say so. The prompt
/// sent to the model is identical either way — only the human-facing wrapper
/// differs, the same way a manual compaction already wraps the summary in a
/// note when the user typed instructions.
pub enum Trigger<'a> {
    /// A user-typed `/compact`, with whatever instructions (possibly empty)
    /// they added.
    Manual { instructions: &'a str },
    /// The token-usage threshold was crossed; nobody asked for this.
    Auto,
}

/// The request that asks `model` to summarize `messages`. `messages` is a
/// model view — [`message_view::model_view`] output, truncated at whatever
/// `cutoff` this compaction targets — so failure records, which are
/// transcript-only, never reach the summarizer.
pub fn summary_request<'a>(
    model: String,
    max_completion_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    messages: impl IntoIterator<Item = &'a HistoryEntry>,
    instructions: &str,
) -> ChatCompletionRequest {
    let mut task = String::new();
    if !instructions.is_empty() {
        task.push_str(
            "The user asked for this compaction and added the following. Follow it \
             where it narrows what to keep, but never at the cost of leaving the agent \
             unable to resume:\n",
        );
        task.push_str(instructions);
        task.push_str("\n\n");
    }
    task.push_str("<transcript>\n");
    task.push_str(&transcript(messages));
    task.push_str("</transcript>\n");
    ChatCompletionRequest {
        model,
        messages: vec![
            RequestMessage::System(SystemMessage(COMPACTION_PROMPT.to_string())),
            RequestMessage::User(UserMessage::text(MessageId::new(), task)),
        ],
        tools: Vec::new(),
        max_completion_tokens,
        temperature: None,
        reasoning_effort,
    }
}

/// The `/compact` line as the transcript records it. This is the only trace of
/// what the user typed: it falls before the new boundary, so the model never
/// sees it — the summary carries the instructions instead. Manual triggers
/// only; an automatic compaction has no typed line to record.
pub fn command_message(instructions: &str) -> Message {
    let text = if instructions.is_empty() {
        "/compact".to_string()
    } else {
        format!("/compact {instructions}")
    };
    Message::User(UserMessage::text(MessageId::new(), text))
}

/// The summary, which is also the new boundary: it records `cutoff` — the
/// last message it covers — so a later [`message_view::model_view`] call
/// knows what this summary does and does not protect.
pub fn summary_message(cutoff: MessageId, trigger: Trigger, summary: &str) -> Message {
    let content = match trigger {
        Trigger::Manual { instructions } if !instructions.is_empty() => {
            format!("[compacted at the user's request: {instructions}]\n\n{summary}")
        }
        Trigger::Manual { .. } => summary.to_string(),
        Trigger::Auto => {
            format!(
                "[compacted automatically: token usage reached the configured limit]\n\n{summary}"
            )
        }
    };
    custom(
        COMPACTION_KIND,
        Some(CustomRole::User),
        content,
        Some(cutoff),
    )
}

/// What is recorded when no summary could be produced. It is *not* a boundary
/// and it is transcript-only (no role), so the model view is untouched — the
/// transcript keeps it as an honest account of why nothing happened.
pub fn failure_message(reason: &str) -> Message {
    custom(
        COMPACTION_FAILED_KIND,
        None,
        format!("Compaction failed, so the conversation was left as it is: {reason}"),
        None,
    )
}

fn custom(
    kind: &str,
    role: Option<CustomRole>,
    content: String,
    cutoff: Option<MessageId>,
) -> Message {
    Message::Custom(CustomMessage {
        message_id: MessageId::new(),
        kind: kind.to_string(),
        role,
        content,
        created_at: jiff::Timestamp::now(),
        cutoff,
    })
}

fn transcript<'a>(messages: impl IntoIterator<Item = &'a HistoryEntry>) -> String {
    let mut out = String::new();
    for entry in messages {
        match &entry.message {
            Message::User(user) => {
                out.push_str("\n## User\n");
                for part in &user.parts {
                    match part {
                        ContentPart::Text { text } => {
                            out.push_str(text);
                            out.push('\n');
                        }
                        // Images cannot be flattened; noting the gap beats
                        // letting the summary imply the turn was text-only.
                        ContentPart::Image { .. } => out.push_str("[image attachment]\n"),
                    }
                }
            }
            Message::Assistant(assistant) => {
                out.push_str("\n## Assistant\n");
                if !assistant.content.is_empty() {
                    out.push_str(&assistant.content);
                    out.push('\n');
                }
                for call in &assistant.tool_calls {
                    out.push_str(&format!(
                        "\n### Tool call: {} {}\n",
                        call.name,
                        call.arguments.as_deref().unwrap_or("{}")
                    ));
                }
                if assistant.aborted {
                    out.push_str("\n[the user interrupted this]\n");
                }
            }
            Message::Tool(tool) => {
                let (label, body) = match &tool.output {
                    ToolOutput::Ok(text) => ("result", text),
                    ToolOutput::Err(reason) => ("error", reason),
                };
                out.push_str(&format!("\n### Tool {}: {}\n", label, tool.name));
                out.push_str(body);
                out.push('\n');
            }
            // An earlier summary, which is where this view begins.
            Message::Custom(custom) => {
                out.push_str(&format!("\n## {}\n", custom.kind));
                out.push_str(&custom.content);
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "compaction_tests.rs"]
mod tests;
