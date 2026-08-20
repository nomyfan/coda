//! What a compaction may summarize, and the messages its answer becomes.
//!
//! The conversation is flattened to plain text rather than replayed as a
//! conversation — that avoids carrying tool definitions, truncated reasoning
//! continuations, and orphan tool results, for what is really a single
//! instruction: summarize this record.
//!
//! [`cutoff`] is the one rule both manual (idle) and automatic (mid-turn)
//! compaction use to decide what's new since the last one — manual passes
//! `protect: None` (whole history is fair game), automatic passes the
//! in-progress turn. Neither re-derives the answer, so they can't disagree.

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
/// new since the last one.
///
/// `protect` names the turn whose own messages must stay out of the summary
/// (the in-progress turn, for a mid-turn/automatic compaction), or `None`
/// when the whole history is fair game (idle/manual).
///
/// "Nothing new" covers an empty thread, a protected turn with no predecessor,
/// or a target at/before the existing boundary. That last case is also why a
/// turn compacts at most once: `protect` pins the target to the same message
/// while the turn is current, so a successful compaction past it disqualifies
/// the rest of the turn. A *failed* attempt doesn't move the boundary — it
/// isn't [`COMPACTION_KIND`] — so a later check in the same turn retries.
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

/// Resolves a [`cutoff`] result back to its index in the same slice it was
/// read from.
pub fn resolve_cutoff_idx(messages: &[HistoryEntry], cutoff_id: MessageId) -> usize {
    messages
        .iter()
        .position(|entry| entry.message.message_id() == cutoff_id)
        .expect("cutoff always names a message id from the same slice it was read from")
}

/// What prompted a compaction — only the human-facing wrapper text differs;
/// the prompt sent to the model is identical either way.
pub enum Trigger<'a> {
    /// A user-typed `/compact`, with whatever instructions they added.
    Manual { instructions: &'a str },
    /// The token-usage threshold was crossed; nobody asked for this.
    Auto,
}

/// The request that asks `model` to summarize `messages` — a model view
/// ([`message_view::model_view`] output) truncated at this compaction's
/// `cutoff`, so failure records never reach the summarizer.
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

/// The `/compact` line as the transcript records it — falls before the new
/// boundary, so the model never sees it; the summary carries the
/// instructions instead. Manual triggers only.
pub fn command_message(instructions: &str) -> Message {
    let text = if instructions.is_empty() {
        "/compact".to_string()
    } else {
        format!("/compact {instructions}")
    };
    Message::User(UserMessage::text(MessageId::new(), text))
}

/// The summary, which is also the new boundary: it records `cutoff` — the
/// last message it covers — so a later [`message_view::model_view`] knows
/// what it does and doesn't protect.
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

/// Recorded when no summary could be produced. Not a boundary, and
/// transcript-only (no role), so the model view is untouched.
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
