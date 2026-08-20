## Problem

Auto compaction cannot recover a thread whose current turn alone reaches the context limit, because cutoff selection protects the entire in-progress turn and therefore finds nothing to summarize.

## Scope

In scope:

- Let automatic compaction fall back to a cutoff inside the current turn when
  no useful turn boundary remains.
- Prefer preserving the current turn verbatim by compacting at its preceding
  turn boundary.
- Use provider-reported prompt usage as a conservative signal when preserving
  the current turn is already known to be insufficient.
- Preserve a newly submitted task verbatim before its first generation.
- Keep the provider-facing message sequence valid across tool-call batches.
- Reject a malformed provider-facing history before compaction or generation;
  never treat it as merely having nothing new to compact.
- Allow repeated compactions during one exceptionally long turn.
- Keep manual compaction and the persisted `CustomMessage.cutoff` representation working as they do today.

Out of scope:

- Changing how the configured token threshold is calculated.
- Per-message token estimation or retaining a token-sized raw tail.
- Changing the summary prompt, storage schema, wire protocol, or UI.
- Recovering a request after the provider has already rejected it for exceeding its context window; compaction remains proactive, based on the preceding successful request's usage.

## Assumptions

- Auto compaction runs only immediately before `ResumePoint::Generation`.
- At that point a normal history has no outstanding tool calls: either the last model-visible message is the user message opening the task, or the previous assistant tool-call batch has all of its tool results recorded.
- Tool-call IDs are unique within an assistant message. Separate batches do not overlap because the runtime does not generate again until the current batch settles.
- A summary may replace current-turn content, including the opening task, only
  when there is no uncompacted earlier-turn prefix left to summarize and the
  agent has already made progress in the current task; the compaction prompt is
  responsible for retaining enough state to resume.
- Histories restored from storage may be malformed or incomplete. A shared
  validator runs at every provider-generation boundary regardless of usage or
  compaction threshold, and stops the generation path on invalid history.
- Provider usage is optional and may be missing or non-monotonic. Such usage is
  treated as unknown and never forces an intra-turn cutoff.

## Validation Findings

- Question: why does the first long turn fail to compact? Method: inspected `compaction::cutoff` and its tests. Result: with `protect: Some(current_turn)`, selection searches only for a message carrying another turn ID; the existing test explicitly expects `None` when no predecessor exists. Implication: turn ID cannot remain the atomic unit of auto compaction.
- Question: when is auto compaction called? Method: inspected the driver loop. Result: it runs immediately before every generation, after a completed tool-execution state returns to `Generation`. Implication: a completed tool batch is the natural safe intra-turn boundary.
- Question: can a cutoff split a tool batch? Method: inspected request lowering and tool execution. Result: history is replayed directly to providers, and providers reject tool results without their originating assistant tool call. Parallel results may land in any order. Implication: cutoff selection must treat one assistant tool-call message plus all of its results as an atomic region.
- Question: can the existing summary data model express an intra-turn cutoff? Method: inspected `CustomMessage.cutoff` and `message_view::model_view`. Result: the cutoff is a message ID, independent of turn ID, and the model view already reorders a physically later summary ahead of messages after that cutoff. Implication: no persistence or wire-format change is required.
- Question: can recorded usage quantify current-turn growth? Method: inspected
  `AssistantMessage.usage`. Result: each assistant message may carry the
  provider's cumulative `prompt_tokens` for the request that produced it. For
  adjacent assistant messages `Aᵢ` and `Aᵢ₊₁`, the non-negative difference
  `prompt_tokens(Aᵢ₊₁) - prompt_tokens(Aᵢ)` estimates the context added by
  `Aᵢ` and its complete tool-result batch. These intervals align with safe
  cutoff units and can be summed within one uncompacted turn. The estimate
  excludes the opening user message and the newest assistant/tool batch, which
  has no following request usage yet. Implication: a large accumulated
  difference can show that turn-only compaction is insufficient, but a small
  difference cannot show that it is sufficient.
- Question: does choosing a boundary where no tool calls are open guarantee a
  safe compaction? Method: tested the rule against the malformed sequence
  `User -> Assistant(c1, c2) -> Tool(c1)`. Result: choosing `User` as the cutoff
  leaves an incomplete assistant batch after the summary. Implication: safety
  is a property of the provider-facing retained suffix, and malformed input
  must be reported rather than hidden or skipped.
- Question: can `None` represent invalid history safely? Method: followed the
  existing driver control flow after `maybe_auto_compact`. Result: `None`
  currently means “skip compaction”, after which generation proceeds.
  Implication: the planner needs an explicit error result and the driver must
  terminate the generation path on that error.
- Question: is planner validation reached below the auto-compaction threshold?
  Method: followed `maybe_auto_compact`'s fast paths. Result: missing usage or
  usage below the threshold returns before loading history or calling the
  planner. Implication: history validation cannot be owned only by compaction;
  normal request construction must validate independently before every
  `LLMStart`.

## Alternatives Considered

### Always compact through the latest message

At the current driver call site this normally works because generation begins only after tool execution settles. It is the smallest change, but it makes message validity an undocumented runtime precondition. A restored or malformed history could leave orphan tool results after the summary. Rejected in favor of validating the boundary inside the compaction module.

### Preserve the most recent completed tool batch verbatim

This retains more exact recent context, but it may retain the very large tool output that caused the threshold crossing and therefore fail to create enough room. Doing this reliably requires per-message token accounting or estimation. Rejected for now; compacting through the newest complete batch gives predictable relief without adding a tokenizer-dependent policy.

### Always compact through the latest complete batch

This maximizes the amount of context reclaimed and is the most direct fix for
single-turn exhaustion. It unnecessarily summarizes the current turn when an
earlier turn boundary is still available, losing exact current-task context
that the existing behavior deliberately preserves. Rejected in favor of a
turn-first strategy with intra-turn fallback.

### Estimate every message with a local tokenizer or character ratio

This could choose a token-sized retained tail, but OpenAI-compatible providers
may use different or undisclosed tokenizers, and image/tool-schema overhead is
not represented by message text alone. Rejected for this change; actual
provider usage is used only where it establishes a safe lower bound.

### Generate the preferred summary, then use its usage to predict the resulting request

The summary request's token count describes a flattened transcript under a
different system prompt, not the normal provider request. Rejecting that
summary and generating a broader one would add latency and cost while still
depending on an estimate. Rejected in favor of feedback from the next real
generation.

### Keep protecting the whole current turn without an intra-turn fallback

This preserves current work exactly, but it cannot complete a single long-running turn and therefore does not solve the problem.

### Validate only inside the cutoff planner

This catches malformed history when auto or manual compaction actually plans a
cutoff, but the usage fast path skips the planner when usage is absent or below
threshold. Rejected because validation must guard provider requests, not only
compaction attempts.

## Components

- `compaction` cutoff planner: first tries the safe boundary preceding the
  current turn, then finds the newest model-valid prefix inside that turn only
  when the preferred boundary has no new content to cover; before returning it
  validates the current model view and the retained suffix for the candidate.
- `message_view`: materializes the latest summary followed by messages after its
  recorded cutoff, owns the shared tool-call/result sequencing rules, and
  validates that exact sequence before provider generation or cutoff planning.
- Runtime auto-compaction policy: supplies the current turn and protects a
  just-appended opening user message from the intra-turn fallback; it also
  derives the conservative current-turn growth signal from recorded usage.

## Interfaces

The exact Rust names may be adapted during implementation, but the boundary should have this shape:

```rust
pub struct Cutoff {
    pub message_id: MessageId,
    pub history_index: usize,
}

#[derive(Debug)]
pub enum InvalidHistory {
    OrphanToolResult { message_id: MessageId, call_id: String },
    DuplicateToolCall { message_id: MessageId, call_id: String },
    DuplicateToolResult { message_id: MessageId, call_id: String },
    IncompleteToolBatch {
        assistant_message_id: MessageId,
        missing_call_ids: Vec<String>,
    },
}

/// Validates the exact sequence produced by `model_view`. This is the shared
/// trust-boundary check used by normal generation and compaction planning.
pub fn validate_model_view(
    messages: &[HistoryEntry],
) -> Result<(), InvalidHistory>;

/// Prefers the safe prefix before `prefer_before_turn`, then falls back to the
/// newest safe prefix. `protect_from`, when present, and everything after it
/// remain verbatim. `Ok(None)` means neither prefix contains new content;
/// `Err` means the provider-facing history is not safe to send.
pub fn cutoff(
    messages: &[HistoryEntry],
    prefer_before_turn: Option<TurnId>,
    protect_from: Option<MessageId>,
    auto_compact_threshold_tokens: Option<u32>,
) -> Result<Option<Cutoff>, InvalidHistory>;

/// Builds the ordinary provider request history. Validation failure is
/// returned before the caller emits `LLMStart` or opens a provider stream.
impl Agent {
    pub async fn messages(
        &self,
    ) -> Result<Vec<RequestMessage>, InvalidHistory>;
}
```

Returning the index with the ID removes the current second lookup and guarantees that the summary request and recorded boundary refer to the same history snapshot.

Selection is ordered:

1. Materialize the same `message_view::model_view` that a normal generation
   would send and validate its tool protocol. An orphan or duplicate tool
   result, duplicate call ID within a batch, a non-tool message before all
   results arrive, or end-of-view with missing results returns `Err`.
2. Find the newest candidate boundary before `prefer_before_turn`. Validate the
   provider-facing retained sequence formed by the proposed summary as a user
   message followed by every model-visible message after the candidate. A
   candidate is eligible only when this suffix contains complete tool batches
   and no orphan result.
3. When that boundary contains new content, walk adjacent assistant messages
   in the uncompacted part of the current turn. Sum each monotonic
   `prompt_tokens` difference; missing or decreasing usage breaks that estimate
   chain. If the known accumulated growth is below the threshold, use the
   preferred turn boundary. If it already reaches the threshold, skip the turn
   boundary because the measured batches retained inside the turn are already
   too large.
4. Otherwise find the newest candidate boundary before `protect_from`, applying
   the same retained-suffix validation. This is the intra-turn fallback.
5. Return `Ok(None)` when neither candidate contains new content.

At the runtime call site:

- Pass the current turn as `prefer_before_turn`.
- Pass the configured auto-compaction threshold so the planner can apply the
  conservative prompt-growth check. Manual compaction passes no threshold.
- If the newest model-visible message is the user message that has just opened
  a generation, pass its ID as `protect_from`; this prevents the fallback from
  summarizing a task that has not started.
- Otherwise pass `None`; if the preferred turn boundary is unavailable, the
  planner may select the latest completed tool batch inside the current turn.
- On `Err(InvalidHistory)`, do not call `handle_generation`: end the current
  root turn with `AgentEvent::Error`, or return the equivalent failed tool reply
  for a sub-agent. The diagnostic identifies the offending message/call.
- `handle_generation` always handles the `Agent::messages` result before it
  constructs or emits `LLMStart`. This check is unconditional: it still runs
  when `maybe_auto_compact` returned early for absent or below-threshold usage.

Manual compaction passes `None` for both preferences and the threshold, and
retains its current all-available-history behavior. It maps invalid history to
a distinct `CompactError` and does not append the `/compact` command or call the
summarizer.

## Data Model

No persisted data changes. A successful summary continues to store the selected message ID in `CustomMessage.cutoff`. Successive summaries in one turn form a logical chain: the new summary request sees the previous summary plus the newly completed tail, and the new summary supersedes the previous one in `message_view`.

The cutoff planner derives transient state while scanning the current model view:

- A validation state with either no open batch or one assistant batch carrying
  its message ID, expected call IDs, and results already seen. A non-tool
  message may follow only after every expected result has arrived.
- Candidate boundaries paired with their physical history indices. Candidate
  safety is decided by validating `summary + retained suffix`, not by assuming
  that the summarized prefix was complete.
- The latest summary boundary and optional protected-tail position.
- The last safe boundary before the current turn, when one exists.
- Per-batch token estimates derived from adjacent provider-reported
  `prompt_tokens` in the current turn. They are transient policy input and are
  not persisted separately; an intervening compaction summary resets the
  cumulative baseline.

## Load-Bearing Decisions

- A turn boundary remains the preferred cutoff because it preserves the
  current task's exact instructions, reasoning continuation, and tool context.
  The safe-prefix planner falls back to a boundary inside the turn only when
  the preferred boundary does not contain anything new to compact, or recorded
  prompt growth proves that retaining the turn already exceeds the threshold.
- Provider-valid message prefixes, rather than turns, are the underlying safety
  unit. This makes the intra-turn fallback and repeated compaction possible
  without producing orphan tool results.
- A task-opening user message is protected only until the agent has made progress. This avoids summarizing a fresh instruction because of usage inherited from the previous turn, without making the entire turn permanently ineligible.
- Once intra-turn fallback is required, it covers through the newest complete
  batch rather than retaining a raw recent batch. This prioritizes reliably
  freeing context over verbatim retention; a token-budgeted tail can be added
  later behind the same planner interface.
- Tool-protocol validation belongs in `message_view`, the shared owner of the
  provider-facing sequence. Both normal generation and `compaction` reuse it;
  the planner additionally owns candidate retained-suffix validation and cutoff
  policy.
- Invalid history and no new content are different outcomes. `Ok(None)` permits
  generation to continue, while `Err(InvalidHistory)` terminates it before any
  provider request is built.
- Validation is unconditional at the generation trust boundary. Usage checks
  may skip compaction work, but may never skip `Agent::messages` validation.
- Usage is a one-way override, not an estimate of sufficiency. Adjacent
  assistant-message deltas may force the intra-turn fallback when the known
  current-turn batch growth reaches the threshold; they never choose the
  preferred turn boundary merely because measured growth is small.

## Risks / Open Questions

- Summary quality becomes more important because current-task instructions and recent tool output can be summarized. The first integration test should verify that a single-turn tool loop resumes correctly from the summary, not merely that a summary record exists.
- A small earlier-turn prefix may technically provide a preferred cutoff while
  reclaiming too little context. Without per-message token accounting, the
  planner cannot always know this before sending the next request. The prompt
  growth lower bound catches the cases it can prove, and the next successful
  generation supplies authoritative post-compaction usage. The configured
  threshold's headroom makes the remaining turn-first attempts reasonable, but
  a request can still fail if uncounted recent tool output already occupies
  almost the whole window.
- Provider usage is only known after a successful generation. One individual tool result can be so large that the summarization request itself exceeds the provider context before compaction can help. Solving that requires truncating/externally storing tool output or estimating request size before summarization and is outside this change.
- Reasoning continuation fields attached to an assistant tool-call message must disappear together with that message. Treating the full tool batch atomically provides that guarantee.
- Validation covers the tool-call/result sequencing constraints required for
  compaction safety, not every provider-specific message rule. Provider adapters
  remain responsible for their own schema validation.

## Implementation Roadmap

- [ ] [risk validation] add a runtime test for a thread whose first turn crosses the threshold after a completed tool batch and must continue generating
      Purpose: reproduce the currently unhandled case and prove that resuming from a current-turn summary works
      Verification: the next request starts with the summary, contains no orphan tool result, and the same turn reaches its final answer
- [ ] [core logic] add a shared model-view validator, then replace turn-only cutoff selection with a usage-assisted, turn-first safe-prefix planner, retained-suffix validation, and an explicit invalid-history result
      Purpose: preserve the existing preferred boundary while making message validity, provably insufficient turn retention, fallback, “nothing new”, and malformed history distinct and testable in isolation
      Verification: shared-validator tests cover valid single and parallel batches plus missing/duplicate/orphan/cross-batch tool results; planner tests cover preference for a previous turn, aggregation of adjacent assistant usage deltas, forced fallback when known batch growth reaches the threshold, missing/non-monotonic usage, reset across a summary, first-turn fallback, a fresh protected user message, candidate suffix validation, an existing summary, and repeated same-turn cutoffs
- [ ] [integration] make provider request construction validate unconditionally, then make the driver supply the preferred current-turn boundary and protect only a task-opening user message from fallback
      Purpose: enable intra-turn and repeated automatic compaction without discarding the current turn when an earlier boundary is useful
      Verification: runtime tests cover previous-turn preference, usage forcing an intra-turn cutoff, actual post-compaction usage causing a later fallback, first-turn compaction, a new turn retaining its raw opening task, two successful compactions in one long turn, and malformed histories with missing and below-threshold usage both ending before `LLMStart`
- [ ] [regression] run the full Rust checks required by the workspace
      Purpose: catch request-shape, persistence, and runtime regressions
      Verification: `cargo clippy` and `cargo test` pass
