## Problem

Trigger context compaction automatically when a model's token usage crosses a threshold — including mid-turn, not just at the idle boundary manual `/compact` requires today — while keeping one shared notion of "what a compaction may summarize" for both triggers. See [../requirement/auto-compaction.md](../requirement/auto-compaction.md).

## Scope

**In**
- A shared, crate-level (`coda_agent`) compaction-boundary rule that both the existing manual `/compact` (idle, storage-driven) and a new automatic trigger (mid-turn, live-agent-driven) call for exactly the same "what counts as new since the last compaction" decision.
- The automatic trigger itself: checked on the root thread only, right before every LLM call, comparing the last known `CompletionUsage.total_tokens` against a per-model threshold.
- A per-model `auto_compact_threshold` config field (`coda-server.toml`), defaulting to 80% of `context_window` when unset.
- Extending the compaction summary message to carry an explicit cutoff, so a summary written mid-turn can protect the in-progress turn's own messages while still being appended (as it must be) after them.

**Out**
- Everything already out of scope per the requirement doc: an on/off toggle, custom auto-compaction prompts, handling a still-over-threshold turn after one compaction attempt, and sub-agent threads.
- Any change to the compaction summarization prompt itself (`compaction-prompt.md`) or to what the LLM is asked to do — only *which messages* it's asked to summarize, and *how the result is recorded*, change.
- Web dashboard changes. The existing context-usage indicator keeps working via the same wire shape; see Risks for the one edge case this leaves unpolished.

## Assumptions

- `messages.payload` is a `Jsonb` column holding the serialized `Message` enum (`app/coda_server/src/schema.rs:14`); confirmed by reading the schema, not just inferred. Adding an optional field to `CustomMessage` is therefore a pure additive Rust/serde change — old rows deserialize it as `None` — with no SQL migration.
- `seq` is assigned incrementally at persistence time as literally "the index into the checkpoint's message vector" (`storage.rs:828`, and the `stored_count + offset` writes at `storage.rs:1171`/`1431`). Confirmed by reading, not assumed. This is why the design never inserts a message at a computed earlier position — persistence only ever appends, so anything conceptually "earlier" has to be expressed as metadata on a later-appended message, not as physical placement.
- `HistoryEntry`'s `Vec` order in memory equals `seq` order (both are strictly append order; nothing reorders the in-memory vector after the fact) — this is what lets index arithmetic on `Agent::history()` stand in for a `seq` comparison without a storage round-trip.
- A turn's messages are contiguous in a thread's history (no interleaving between turns on one thread) — true today because `current_turn` only advances on a new user message and every append in between is stamped with it (`agent.rs:550-689`).

## Validation Findings

| Question | Method | Result | Implication |
|---|---|---|---|
| Does manual `/compact` run against the live in-memory `Agent`, or storage directly? | Read `hub.rs:996-1079` and `server.rs:344-401` | Storage directly — `compact()` loads a DB checkpoint, calls the LLM, and commits via a message-count compare-and-swap (`storage.commit_compaction`). The live agent, if any, reloads from storage the next time its thread wakes (`driver.rs:502-527`). | This path is safe only because the session is idle; it cannot be reused as-is for a live, in-flight turn — confirms the requirement doc's constraint. Auto-compaction must instead mutate the live `Agent`'s in-memory history directly, like any other message the driver appends, and ride the driver's existing checkpoint-save path. |
| Can a compaction summary be inserted at its "logical" position instead of the tail? | Read `storage.rs` `seq` assignment (`828`, `1171`, `1431`) | No — `seq` is assigned as the next free index at write time; persistence is append-only. | The cutoff must be carried as metadata on a tail-appended message, not expressed by where the message physically sits. This directly shapes the `model_view` redesign below. |
| Does `coda_server::compaction` already depend on `coda_agent`, making a move feasible? | Read `compaction.rs:11-16` | Yes — it already imports `coda_agent::HistoryEntry` and `coda_agent::message_view::{COMPACTION_KIND, COMPACTION_FAILED_KIND}`. | Moving the module down into `coda_agent` removes an existing dependency-direction awkwardness rather than creating one; `coda_server` depends on `coda_agent`, never the reverse. |

## Alternatives Considered

**Where the cutoff lives: explicit pointer on the summary message (chosen) vs. splice the message into its logical position.** Splicing would keep `model_view` exactly as it is today (boundary = physical position), but it's incompatible with how persistence assigns `seq` (validated above) — `save_checkpoint` only ever appends the tail past what's already stored, so a spliced-in message would corrupt the append-only invariant that rewind's `seq >= target` and fork's `retained_turns` (`storage.rs:695-740`) both depend on. An explicit `cutoff: Option<MessageId>` field keeps storage untouched and confines all the new complexity to a read-time view construction.

**Where the auto-trigger runs: inside the driver's existing async turn loop (chosen) vs. suspend the turn and route through the idle-gated hub `compact` command.** Routing through the hub would reuse the manual path's storage-CAS machinery as-is, but it means inventing a new suspend/resume protocol for a turn that isn't waiting on user input — the turn would have to fully exit the driver, let the hub run `compact`, then re-enter, doubling the state machine's cases for something the driver can do in one more `await` using cancellation machinery (`self.cancel`) that already exists for the main LLM call. Running it in-process is more code in `coda_agent`, but it's the same kind of code already there.

**Where the shared logic lives: keep it in `coda_server`, have `coda_agent` call back into it (rejected — wrong dependency direction; `coda_agent` cannot depend on `coda_server`) vs. move it into `coda_agent` (chosen).** Also the more accurate home architecturally: deciding what a thread's history summarizes to is thread/history logic, not an HTTP-server concern — `coda_server`'s slice of the feature becomes "read config, read/write storage, call the provider," identical in shape to what it already does for manual compaction today.

**Threshold defaulting: computed at check-time in `coda_agent` (needs raw `context_window` threaded down) vs. resolved once at config-load time in `coda_server` (chosen).** Resolving once keeps `coda_agent` ignorant of the 80% policy — it only ever reads one concrete `u32` — matching how `max_completion_tokens` is already resolved once in `config.rs` and handed down as a plain value on `ModelProfile`.

**Finding "the last recorded usage": scan `Agent::history()` backward each check (chosen) vs. a cached `last_usage` field on `AgentState`, updated wherever an assistant message is appended.** A cache avoids the scan but is new mutable state with its own lifecycle to get right — cleared or not on `restore_history`, kept in sync at every append site, one more thing a future append path can forget to update. `AgentState.messages` is not large enough (turns, not raw tokens) for a backward scan to matter, and the scan is only ever needed once per `ResumePoint::Generation` entry, not per message. No new field, no new invariant to maintain.

## Components

- **`coda_agent::compaction`** (new module, moved from `app/coda_server/src/compaction.rs`) — decides what a compaction may summarize and builds the messages that record it. Owns the one rule ("what's new since the last compaction, given what must stay protected") that both triggers call, so they can never disagree about a boundary.
- **`coda_agent::message_view`** (extended) — the model's window onto a thread's history. Generalizes from "everything physically after the last compaction message" to "everything the last compaction message's recorded cutoff excludes", reordering so the summary always leads the view it belongs to regardless of where it was actually appended.
- **`coda_agent::runtime::driver::AgentLoop`** (extended) — where the automatic trigger lives: a check inserted before every LLM call on the root thread, using data the loop already has (history, current turn, model profile).
- **`app/coda_server` config + session wiring** (extended) — resolves the per-model threshold from TOML (with the 80% default) once, and threads it down to `ModelProfile` alongside the fields it already carries.
- **`app/coda_server::compaction`/`hub.rs`/`server.rs`** (thinned) — keeps only what's genuinely server-specific: reading the workspace's provider handle, the storage compare-and-swap commit, and the idle gate. Everything about *what to summarize* delegates to `coda_agent::compaction`.

## Interfaces

```rust
// coda_agent::compaction

/// What a compaction may summarize right now: `protect` names the turn (if
/// any) whose own messages must stay out of the summary — the in-progress
/// turn for a mid-turn/automatic compaction, `None` when nothing needs
/// protecting (the idle/manual case, where the whole history is fair game).
///
/// Returns `None` when there is nothing new to summarize since the last
/// compaction: an empty thread, a protected turn with no predecessor turn at
/// all, or (this is what keeps a repeated over-threshold check from compacting
/// twice in a row) a cutoff that would not move the existing boundary forward.
/// Both triggers call this and only this to decide "is there anything to do" —
/// callers never compare boundaries themselves.
pub fn cutoff(messages: &[HistoryEntry], protect: Option<TurnId>) -> Option<MessageId>;

/// The request that asks `model` to summarize everything `cutoff` covers —
/// the model view of `messages` truncated at `cutoff`, reusing the same
/// boundary rule recursively for a second-or-later compaction. Same shape as
/// today's `summary_request`.
pub fn summary_request<'a>(
    model: String,
    max_completion_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    messages: impl IntoIterator<Item = &'a HistoryEntry>,
    instructions: &str,
) -> ChatCompletionRequest;

/// The summary message that becomes the new boundary, recording `cutoff` so a
/// later `model_view` call knows what this summary does and does not cover.
/// `trigger` distinguishes the human-facing transcript wording (a typed
/// `/compact` line's instructions vs. an automatic, silent trigger) without
/// changing what's sent to the model.
pub fn summary_message(cutoff: MessageId, trigger: Trigger, summary: &str) -> Message;

/// What is recorded when no summary could be produced — unchanged from today,
/// transcript-only and not a boundary.
pub fn failure_message(reason: &str) -> Message;
```

```rust
// coda_agent::message_view

/// The model's window on `messages`: the last compaction summary (if any)
/// leading, followed by everything its recorded cutoff excludes — in original
/// order — minus transcript-only records. A summary with no recorded cutoff
/// (every summary written before this change) falls back to its own physical
/// position, so already-persisted threads keep behaving exactly as they do
/// today.
pub fn model_view(messages: &[HistoryEntry]) -> impl Iterator<Item = &HistoryEntry> + '_;
```

```rust
// coda_agent::runtime::driver::AgentLoop (private)

/// Checked once per entry into `ResumePoint::Generation`, root thread only.
/// Compares the last recorded usage — found by scanning `self.agent.history()`
/// backward for the most recent `Message::Assistant` carrying `usage` — against
/// the profile's threshold; on exceed, asks `compaction::cutoff` whether
/// there's anything new to summarize and, if so, runs the summarization
/// request — raced against `self.cancel` exactly like the main generation call
/// — before the next LLM request goes out. Appends the resulting message the
/// same way any other mid-turn message is appended; nothing about it is
/// announced as an event.
///
/// On failure, appends only `failure_message`: no boundary moves and nothing
/// is recorded to suppress a later attempt in this same turn — see the
/// Load-Bearing Decisions entry on retry semantics.
async fn maybe_auto_compact(&mut self);
```

Trust boundary: none newly introduced. The auto-compact threshold is operator-configured (`coda-server.toml`), not user input; the only new data crossing from an external source is the LLM's summary text, which was already crossing that boundary for manual compaction and is handled the same way (stored as opaque transcript content, never interpreted).

## Data Model

- **`CustomMessage`** (`coda_core::llm`) gains one field: `cutoff: Option<MessageId>`. Populated only for `kind == COMPACTION_KIND`; every other kind (including `COMPACTION_FAILED_KIND`) leaves it `None`, unchanged from today. Additive JSON field on the existing `Jsonb` payload column — no migration.
- **`HistoryEntry.turn_id`** is unchanged in meaning, but a compaction summary written mid-turn now carries the turn it was *appended during* (the in-progress turn), not the turn range it *covers* (recorded separately via `cutoff`). Ownership stays with whichever thread's `AgentState` holds it; nothing shares it across threads.
- **`ModelConfig`** (`app/coda_server/src/config.rs`) gains `auto_compact_threshold: Option<u32>`, parsed and validated the same way `max_completion_tokens` is (positive, ≤ `context_window`). **`ProviderHandle`** resolves it once to a concrete value (configured value, or `context_window * 80 / 100`). **`ModelProfile<P>`** (`coda_agent::agent`) gains the resolved `auto_compact_threshold_tokens: u32`, threaded through `open_session` in `server.rs` the same way `max_completion_tokens` already is.

## Load-Bearing Decisions

- **Cutoff is metadata on a tail-appended message, never physical placement.** Forced by how `seq` is assigned (validated above); reversing this later would mean redesigning how `save_checkpoint` assigns `seq`, not a local fix.
- **`coda_agent::compaction` is the single owner of "what's new since the last compaction."** Both triggers call it and neither re-derives the answer. This is what "unify the cutoff logic" cashes out to concretely — a shared function with one signature, not two implementations that are merely similar.
- **The automatic trigger lives inside the driver's turn loop, not behind the hub's command layer.** Auto-compaction is therefore never observable as a distinct `SessionCommand` or wire event — it's an internal step of turn processing, the same way appending a tool message is. Revisiting this later (e.g. to expose it as a live event) is a driver-level change, not a protocol one, but it *is* a protocol change if a future requirement wants the client to see it happen in real time rather than only in the persisted transcript.
- **`model_view`'s fallback for a summary with no recorded `cutoff` treats it as "boundary = own physical position"** (today's behavior). This makes the change backward-compatible with every already-persisted compaction summary without a data migration, at the cost of `model_view` carrying two code paths indefinitely (or until a migration backfills `cutoff` on old rows, which this brief does not propose).
- **A failed compaction attempt does not suppress the next detection point in the same turn.** `cutoff()`'s "existing boundary" is defined by the last *successful* `COMPACTION_KIND` summary (the only kind that carries a boundary at all); `failure_message` carries none. So after a failure, `cutoff()` still returns `Some` at the next `ResumePoint::Generation` entry if usage is still over threshold, and `maybe_auto_compact` tries again — a fresh attempt, not a retry loop within one detection point, and bounded by however many generation rounds the turn has left. This is a deliberate choice over adding a per-turn "already failed, don't try again" flag on `AgentState`: that flag would be new mutable state to keep in sync (cleared on the next user turn, surviving a crash/reload correctly, etc.) to prevent a failure mode — a flaky provider call — that a plain retry-on-next-detection already handles for free. The requirement doc's "no retry" wording has been sharpened to describe exactly this: no retry *within* one detection, not a suppression of later, independent detections. Confirmed with the requirement's author.

## Risks / Open Questions

- **Reordering the model view (summary first, then the rest) is a coherence bet, not something a unit test can confirm.** It's the right call on paper — the alternative leaves a summary of "everything before this turn" sitting in the middle of this turn's own tool-call sequence — but it should be eyeballed against a real model's behavior once (roadmap step 6), not just trusted from the design.
- **The web dashboard's context-usage indicator scans backward for the compaction marker and reads the first usage figure after it.** In the gap between an auto-compaction landing and the next assistant message arriving (both within the same turn), that scan finds no usage yet. This self-heals the moment the next LLM response lands — which happens immediately, since that's what the compaction was for — but leaves a brief stale/zero reading. Out of scope for this brief; flagging so it isn't mistaken for a bug later.
- **Manual `/compact` run twice with nothing new in between now becomes `CompactionEmpty` instead of re-summarizing "a summary of a summary."** A direct consequence of sharing `compaction::cutoff`'s "nothing new" guard. Arguably a bug fix (today's behavior wastes an LLM round-trip on a pointless summary), but it is an observable behavior change worth the user's attention.
- **Fork/rewind were reasoned about, not exercised.** Fork cuts only ever anchor at a user message (`retained_turns`, `storage.rs:707-719`) and rewind cuts contiguously by `seq` — analysis says a compaction summary's turn-tagging (append turn, not covered range) doesn't interact badly with either, but this should get a dedicated test rather than resting on the reasoning alone.

## Implementation Roadmap

- [ ] [core logic] Move `compaction.rs`'s message/request-building functions into a new `coda_agent::compaction` module; add `cutoff: Option<MessageId>` to `CustomMessage`; generalize `message_view::model_view` to resolve the boundary via the summary's recorded `cutoff`, reordering the view (summary leading, then everything the cutoff excludes, in original order), with a fallback to today's physical-position rule when `cutoff` is absent.
      Purpose: establishes the single, crate-shared boundary rule both triggers will call.
      Verification: extend `message_view.rs`'s existing unit tests to cover a cutoff before the summary's own physical position, the reordering, and the no-`cutoff`-recorded legacy fallback.
- [ ] [core logic] Add `compaction::cutoff(messages, protect: Option<TurnId>) -> Option<MessageId>`.
      Purpose: the shared decision function; eliminates the repeat-trigger risk structurally rather than by a separate guard each caller has to remember.
      Verification: unit tests — no-protect returns the last message; a protected turn with a predecessor returns the previous turn's last message; a protected turn with no predecessor (thread's very first turn) returns `None`; a cutoff at or before the existing boundary returns `None`.
- [ ] [integration] Point `server.rs`'s `compact()` and `hub.rs`'s `handle_compact` at the moved `coda_agent::compaction` functions (`cutoff(messages, None)` for the idle case); `summary_message` now takes the resolved cutoff.
      Purpose: proves the shared logic is a drop-in replacement for the existing, tested manual path before any new behavior is added on top.
      Verification: existing manual-compaction tests pass, except the documented "nothing new" no-op case, which gets its own updated test.
- [ ] [config] Add `auto_compact_threshold: Option<u32>` to `ModelConfig`/TOML parsing (validated against `context_window`, mirroring `parse_max_completion_tokens`); resolve it on `ProviderHandle` (configured value, or 80% of `context_window`); thread it to `ModelProfile.auto_compact_threshold_tokens` in `open_session`.
      Purpose: gets the threshold from TOML to where the driver can read it, keeping the 80%-default policy in `coda_server`.
      Verification: config-parsing tests for unset (→ 80%), explicit value, and the exceeds-`context_window` validation error.
- [ ] [core logic] In `driver.rs`, add `maybe_auto_compact`, called before every `ResumePoint::Generation` step when `is_root_thread` is true: compare last recorded `CompletionUsage.total_tokens` to the threshold, call `compaction::cutoff` with the current turn protected, and on `Some`, run the summarization request raced against `self.cancel`, appending the result via `self.agent.add_message`.
      Purpose: the actual auto-trigger; built last because it depends on every earlier step.
      Verification: a driver-level test using a scripted `LLMProvider` that reports `CompletionUsage` over threshold mid-turn — assert the summary's `cutoff` points at the previous turn's last message while its `turn_id` is the current turn, the current turn's own messages stay visible in `Agent::messages()` unchanged, no compaction fires on a sub-agent thread, and a second over-threshold check within the same turn does not compact again *after a successful compaction* (boundary moved, nothing new). Separately, a scripted summarization failure followed by a second over-threshold check in the same turn *does* attempt again (boundary never moved) — asserting the chosen retry semantics explicitly, not just the success path.
- [ ] [verification] `cargo clippy` and `cargo test` across the workspace; one manual run of a long multi-tool-call session against a real provider to eyeball post-compaction coherence.
      Purpose: project-standard correctness gate, plus a sanity check on the one judgment call (reordering) unit tests can't fully cover.
      Verification: clean clippy/test; transcript reviewed by hand.
