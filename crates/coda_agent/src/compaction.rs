//! What a compaction may summarize, and the messages its answer becomes.
//!
//! The conversation is flattened to plain text rather than replayed as a
//! conversation — that avoids carrying tool definitions, truncated reasoning
//! continuations, and orphan tool results, for what is really a single
//! instruction: summarize this record.
//!
//! [`cutoff`] is the one rule both manual (idle) and automatic (mid-turn)
//! compaction use to decide what's new since the last one. Automatic
//! compaction prefers a complete turn boundary, then falls back to a complete
//! tool-batch boundary when the current turn itself has grown too large.

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cutoff {
    /// Entries to take from the full model view of the same history snapshot.
    pub model_view_len: usize,
    /// Physically latest included message; persisted as the summary boundary.
    pub coverage_message_id: MessageId,
}

/// Chooses what a compaction may summarize without leaving an invalid tool
/// sequence behind. Automatic compaction prefers the boundary before
/// `prefer_before_turn`, unless measured growth within that turn already
/// reaches `auto_compact_threshold_tokens`; it then falls back to the newest
/// safe boundary before `protect_from`. Manual compaction omits all three
/// policy inputs.
pub fn cutoff(
    messages: &[HistoryEntry],
    prefer_before_turn: Option<TurnId>,
    protect_from: Option<MessageId>,
    auto_compact_threshold_tokens: Option<u32>,
) -> Result<Option<Cutoff>, message_view::InvalidHistory> {
    let visible: Vec<_> = message_view::model_view_indexed(messages).collect();
    message_view::validate_messages(visible.iter().map(|(_, entry)| &entry.message))?;

    let first_new = visible
        .first()
        .is_some_and(|(_, entry)| message_view::is_compaction_summary(&entry.message))
        as usize;
    let cutoff_at = |position: usize| {
        let (_, coverage) = visible[..=position]
            .iter()
            .max_by_key(|(history_index, _)| history_index)
            .expect("a cutoff position includes at least one message");
        Cutoff {
            model_view_len: position + 1,
            coverage_message_id: coverage.message.message_id(),
        }
    };
    // A candidate position is only safe when the physically-latest message it
    // covers (the watermark `cutoff_at` will persist) still precedes every
    // message the retained suffix keeps. Without this, a reordered summary
    // included in the covered set — always physically newer than the
    // "leftover" messages it protects — could outrank an *excluded* leftover
    // still sitting in the suffix, and `model_view`'s tail search would then
    // sweep that leftover away too, though it was never actually summarized.
    let leaves_valid_suffix = |position: usize| {
        let suffix = &visible[position + 1..];
        if message_view::validate_messages(suffix.iter().map(|(_, entry)| &entry.message)).is_err()
        {
            return false;
        }
        let covered_max = visible[..=position]
            .iter()
            .map(|(history_index, _)| *history_index)
            .max();
        let suffix_min = suffix.iter().map(|(history_index, _)| *history_index).min();
        covered_max
            .zip(suffix_min)
            .is_none_or(|(covered, retained)| covered < retained)
    };

    if let Some(turn) = prefer_before_turn
        && let Some(position) = visible.iter().rposition(|(_, entry)| entry.turn_id != turn)
        && position >= first_new
        && leaves_valid_suffix(position)
    {
        let retaining_turn_is_too_large = auto_compact_threshold_tokens
            .zip(prompt_growth(&visible, turn))
            .is_some_and(|(threshold, growth)| growth >= u64::from(threshold));
        if !retaining_turn_is_too_large {
            return Ok(Some(cutoff_at(position)));
        }
    }

    // Fails closed: `protect_from` should always resolve against `visible`
    // (both are drawn from `messages`), but if it doesn't — a caller passing
    // a stale or foreign id — treat it as "everything is protected" rather
    // than panicking or silently falling back to unprotected.
    let limit = match protect_from {
        None => visible.len(),
        Some(message_id) => visible
            .iter()
            .position(|(_, entry)| entry.message.message_id() == message_id)
            .unwrap_or(first_new),
    };
    for position in (first_new..limit).rev() {
        if leaves_valid_suffix(position) {
            return Ok(Some(cutoff_at(position)));
        }
    }
    Ok(None)
}

fn prompt_growth(visible: &[(usize, &HistoryEntry)], turn: TurnId) -> Option<u64> {
    let mut previous = None;
    let mut growth = 0_u64;
    for (_, entry) in visible {
        if entry.turn_id != turn {
            continue;
        }
        let Message::Assistant(assistant) = &entry.message else {
            continue;
        };
        let prompt_tokens = assistant.usage.as_ref()?.prompt_tokens;
        if let Some(previous) = previous {
            growth = growth.checked_add(u64::from(prompt_tokens.checked_sub(previous)?))?;
        }
        previous = Some(prompt_tokens);
    }
    Some(growth)
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
