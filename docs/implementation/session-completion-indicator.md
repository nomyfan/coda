## Problem

When a background session's turn finishes while the client isn't attached to it (the user switched to another session), the sidebar's running indicator freezes on "running" instead of updating, and there is no way for the user to notice a background session finished at all. The same gap shows up in reverse on a hard page refresh: the client's local state resets to nothing, and since the catalog (`list_workspaces`) has never carried a live signal either, a session that's still genuinely running shows *nothing* until it eventually settles. Give the sidebar an authoritative signal for what a session is doing right now — sourced from wherever that fact actually lives (the database for a settled-but-unseen outcome, the hub's live state for a turn still in flight) — correct after any reload or reconnect, not just once a turn happens to end.

## Scope

In:
- A persisted per-session "unseen outcome" (`completed` / `failed`) recorded when a turn settles with no client attached, covering normal completion, `Aborted`, and `Error`.
- Surfacing it through the existing session catalog (`list_workspaces`) so it's correct after a reload/reconnect.
- A near-real-time push so it appears without a manual refresh, for any connection currently open on the same server process.
- Clearing it (and correcting the stale "running" cache) the moment a client attaches to that session.
- Sidebar rendering: a static green dot for `completed`, static red for `failed`.
- A session's live running state (the hub's in-memory `turn_running`, not the database) merged into the same catalog field as the unseen outcome, so a fresh connect/reconnect/refresh shows the correct indicator immediately — not just once the turn eventually settles.

Out:
- Suspended-for-approval turns — already covered by the existing `has_pending_approval` catalog field; this mechanism explicitly skips them.
- Per-workspace-scoped delivery — the push is broadcast to every connection on the process, unfiltered, matching how `list_workspaces` already hands every connection the full catalog.
- Propagating the "cleared" state live to *other* tabs attached to a different connection — they pick it up on their next catalog fetch/reconnect, same as `has_pending_approval` today.
- Distinguishing `Aborted` from `Error` visually — both render as `failed`/red.
- Live (no-refetch) propagation of a session *starting* to run — e.g. another tab attaches to a background session and sends a task; this tab won't show it as running until its own next `list_workspaces` fetch. Symmetric with the push already built for *stopping*, and cheap to add later given the broadcast infrastructure already exists, but refresh/reconnect correctness was the reported problem, not sub-second cross-tab propagation of a start.

## Assumptions

- Single coda-server process, not multi-tenant — an unfiltered process-wide broadcast is acceptable (matches the existing trust model: `list_workspaces` already exposes the whole catalog to every connection with no ACL).
- A dropped live-push notification (broadcast lag) is acceptable **only because** the client-side catalog-apply path independently enforces the same invariant the push would have — see Components. The DB write is the source of truth; either delivery path (push or next `list_workspaces` fetch/reconnect) is sufficient on its own.
- New tasks can only be sent by an attached client (`command()` rejects a `conn_id` that isn't `state.attached`, and the one unattended path that can set `turn_running` — `make_live`'s `has_resuming_agents` check — is itself only ever reached from an attach). So a session can't re-enter "running" while unattended without first being attached (and thereby clearing any prior unseen outcome). No case where `unseen_outcome` needs to coexist with a fresh `running: true` for the same session — verified in the roadmap's hub_tests, not just assumed.

## Alternatives Considered

**Where does `unseen_outcome` live client-side: on `OpenedSession` (merged with catalog like `awaitingApproval` is today) or as its own catalog-row field?**

`awaitingApproval` today does `opened ? opened.approvalCount > 0 : session.has_pending_approval` (`sidebar.tsx:606`) — once a session has ever been opened in the tab, its local (potentially stale) count wins over the catalog's authoritative value forever. That's the same class of bug this design fixes for `running`, just latent and unreported for approvals. Piggybacking `unseen_outcome` onto `OpenedSession` would inherit that footgun.

Chosen instead: `unseen_outcome` lives only on the catalog row (`SessionSummaryWire`/store's workspace session list), fed by the initial `list_workspaces` fetch and kept fresh by the live push and by an optimistic clear on open. No `opened ? ... : ...` merge, so there's no stale-cache path to it at all. The live push additionally corrects `OpenedSession.running` when one exists — see Components — which is what actually fixes the reported bug.

**Persist a single nullable `unseen_completed_at` timestamp vs. persist the outcome directly:**

A timestamp compared against "last attached" was considered (Slack-style unread cursor) but rejected: a turn that settles *while* the user is attached and watching would also bump "last completed," and a pure timestamp comparison can't tell that case apart from a real unattended completion. The actual fact that matters — "did this turn end at a moment when nobody was attached" — is only knowable at the instant it happens, inside `run_forwarder`, which already has `state.attached` in hand. So the write is a direct, explicit flag set at that instant (and cleared at the next attach), not a derived comparison.

**Rename `unseen_outcome` (the DB column, the `UnseenOutcome` type) to something broader like `status`, now that the catalog also merges in live running state?**

Rejected at the storage layer, accepted at the wire layer — the two are not the same question. The DB column can only ever hold `completed`/`failed`: a running turn is never persisted, it's read live from the hub at request time. Naming the column `status` would claim a range of values (including `running`) it can never actually contain — a reader would reasonably expect `status = 'running'` to be a real possibility and be wrong. The precedent already in this codebase is `has_pending_approval` (wire) vs. `thread_checkpoints.pending_approval` (DB): the two layers deliberately keep different names, because the wire field is a *computed view* (here, merging two independent sources) and the DB column is a narrower *stored fact*. So `unseen_outcome` / `UnseenOutcome` / `mark_unseen_outcome` / `clear_unseen_outcome` stay exactly as they are at the storage layer; only the catalog/wire layer gains a new, broader field — `status` — that merges the DB's `unseen_outcome` with the hub's live `running_sessions()`, superseding what was originally shipped as `SessionSummaryWire.unseen_outcome`.

## Components

- **`sessions.unseen_outcome` column** — durable record of "this session ended un-watched," survives restarts and reconnects.
- **`WorkspaceStorage::mark_unseen_outcome` / `clear_unseen_outcome`** — the two writes (diesel `UPDATE ... WHERE (workspace_id, session_id)`), mirroring the existing `rename_session`/`touch` pattern.
- **`SessionOpener::mark_unseen_outcome` / `clear_unseen_outcome`** — the hub's only route to storage; `AppOpener` implements them by delegating to `WorkspaceStorage`, matching how every other hub→storage write already goes through this trait.
- **`SessionHub`'s status broadcast (`status_tx: broadcast::Sender<SessionStatusEvent>`)** — process-wide, best-effort fan-out of "a session's unseen outcome changed," independent of any specific session's attachment.
- **`run_forwarder`'s settle branch** — the single place that already knows, atomically, both "the turn just ended" and "is anyone attached" (`state.attached`); it's where the persist + broadcast fire together.
- **`run_connection`'s new `session_status` push** — forwards a received broadcast event to its client as a JSON-RPC notification, the same way it already forwards per-session `RelayEvent`s.
- **Frontend catalog store slice** — holds `status` per session row; updated by catalog fetch, the new push, and locally on open. **Both** of those two server-driven update paths (catalog apply and the `session_status` push handler) also reconcile the matching `OpenedSession.running` to match the incoming `status` — in *either* direction (`completed`/`failed`/explicit-`null` force it `false`, `"running"` forces it `true`; only `undefined`, meaning no real catalog data for the row, leaves it alone). This is deliberately redundant: the push is best-effort and may never arrive, but every reconnect/refetch runs `list_workspaces`, so the correction always has a path that doesn't depend on the push. Without this, `sidebar.tsx:605`'s `running={opened?.running ?? false}` has no fallback to the catalog at all (unlike `awaitingApproval`), so a dropped push would leave the exact staleness this design set out to fix. The reconciliation was one-directional (stop only) in the first pass; `/code-review high` caught that it left the symmetric case — a session opened once in this tab, then started running again elsewhere while unattached — stuck showing idle across every future reconnect. See Deviations.
- **`SessionRelay::running_sessions`** — given a workspace id, the session ids currently `turn_running` in the hub, regardless of attachment. The only way `workspace_catalog()` learns about a session that's running but nobody on *this* connection is attached to — which is every session after a hard refresh, by construction (see Problem).
- **`workspace_catalog()`'s merge** — combines `WorkspaceStorage::list_sessions()` (DB: `unseen_outcome`) with `running_sessions()` (hub: live) into one `status` field per session, `running` taking priority. The two can never legitimately coexist for the same session (see Assumptions), so the priority rule never actually has to arbitrate a real conflict — it only has to pick a value during the narrow window where the two reads disagree because one is stale (see Load-Bearing Decisions).
- **`SessionRow` (sidebar)**: reads `session.status` as a *fallback* for the running indicator too — `opened?.running ?? session.status === "running"` — the same pattern `awaitingApproval` already uses, and what actually fixes the refresh case: a freshly-loaded page has no `OpenedSession` for anything yet, so every session's running dot comes from the catalog until (if ever) it gets opened. `completed`/`failed` render as a third, lowest-priority indicator state, unchanged from before.

## Interfaces

```rust
// coda_core / hub.rs — new outcome type, shared by storage, the trait, and the wire.
pub enum UnseenOutcome { Completed, Failed }
```

```rust
// SessionOpener (hub.rs) — two new methods alongside open/load_messages/rewind/...
/// Record that `key`'s turn just settled with nobody attached. Best-effort:
/// a failed write is logged and swallowed, never blocks the settle path.
fn mark_unseen_outcome<'a>(&'a self, key: &'a SessionKey, outcome: UnseenOutcome)
    -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Clear any unseen outcome recorded for `key`. Called on every successful
/// attach; a no-op (no write issued) when there was nothing to clear.
fn clear_unseen_outcome<'a>(&'a self, key: &'a SessionKey)
    -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
```

```rust
// SessionRelay (hub.rs) — new subscription, sibling to attach/command/detach.
/// A live feed of unseen-outcome changes across every session on this
/// process, for connections to forward to their client. Broadcast and
/// best-effort: a lagging receiver misses events; the catalog stays correct.
fn subscribe_status(&self) -> BoxStream<'static, SessionStatusEvent>;

/// Session ids in `workspace_id` with a turn currently in flight, regardless
/// of attachment. Queried once per `list_workspaces` call, not cached — this
/// is a point-in-time read of the hub, same as `provider_of`.
fn running_sessions<'a>(&'a self, workspace_id: &'a str)
    -> Pin<Box<dyn Future<Output = HashSet<String>> + Send + 'a>>;
```

```ts
// protocol.ts — new push, sibling to event/snapshot/session_evicted. Only ever
// carries a settled outcome — never "running"; see Scope (Out).
type SessionStatusPush = {
  workspace_id: string;
  session_id: string;
  outcome: "completed" | "failed";
};

// protocol.ts — WorkspaceSession's merged field (supersedes the originally
// shipped `unseen_outcome`).
status?: "running" | "completed" | "failed" | null;
```

## Data Model

- **`sessions.unseen_outcome`**: nullable text, `check (unseen_outcome in ('completed', 'failed'))`. Lives directly on `sessions` (not a join like `has_pending_approval`'s `thread_checkpoints` EXISTS), so `list_sessions` just selects it — no new subquery.
- **Ownership of the write**: only `run_forwarder` sets it (to a value) and only `attach` clears it (to `NULL`). Both hold the entry's `Arc<Mutex<EntryState>>` guard across the write — see Load-Bearing Decisions. `clear_unseen_outcome`'s `UPDATE` is scoped with `.filter(sessions::unseen_outcome.is_not_null())` — attach is one of the hottest paths and the overwhelming majority of attaches have nothing to clear; Postgres writes a new tuple version on every `UPDATE` regardless of whether the value actually changes, so the filter avoids a wasted dead tuple on the common case.
- **Client-side**: `status` is a field on the catalog's per-session summary row, not on `OpenedSession`. `OpenedSession.running` remains its own thing, but the new push corrects it as a side effect when the two disagree (see Components).
- **`SessionSummaryWire.status`** is computed, not stored anywhere in this exact shape: `workspace_catalog()` combines the DB's `unseen_outcome` (durable) with `running_sessions()` (live, hub-sourced) into one value per session, `running` taking priority. The DB column and the `UnseenOutcome` Rust type it round-trips through are unchanged — see Alternatives Considered for why the two layers keep different names.

## Load-Bearing Decisions

- **The persist call happens while still holding the entry's async mutex guard**, before the existing `maybe_release` check (`hub.rs:1913-1920`) is even reached. This is a correctness requirement, not a style choice: `attach()` needs that same guard to register a new attachment, so holding it across the write closes the window where a client could attach *between* "we observed nobody's attached" and "we recorded the unseen outcome" — which would otherwise mark a session the user is actively looking at as unseen. The trade-off is a DB round-trip under a lock that other operations on *this session's* entry (not other sessions) must wait out; accepted since it's a single indexed UPDATE on a rare path (once per unattended settle).
- **Broadcast is process-wide and unfiltered**, not scoped per workspace or per subscribed connection. Simpler (no new subscription-management state), consistent with `list_workspaces` already being unscoped, and cheap at this deployment's scale. Revisit if workspaces ever need isolation between clients.
- **`unseen_outcome` is schema-additive** (nullable column, no backfill, no default needed) — consistent with this project's "breaking changes are fine, no compat shims" stance; existing rows just read `NULL`.
- **`status`'s two inputs are read independently, not atomically.** `workspace_catalog()` calls `WorkspaceStorage::list_sessions()` (DB) and `SessionRelay::running_sessions()` (hub) as two separate, unsynchronized reads. A turn that settles in the narrow window between them can produce one stale response — neither `running` nor the fresh outcome, for that one fetch — before self-healing on the next fetch or via the existing `session_status` push for that specific session. Accepted for the same reason broadcast lag is (see Assumptions): the DB write is still the source of truth, this is a freshness gap, not a correctness one, and it requires a turn to settle in the literal microseconds between two async calls to manifest at all.

## Risks / Open Questions

- **Broadcast channel capacity**: needs a size that comfortably absorbs a burst of settles across many sessions without lagging a slow connection under normal load. Pick a generous default (e.g. 256) and treat lag as acceptable (see Assumptions) rather than tuning precisely up front.
- **Multi-tab staleness on clear**: a second tab showing the same session's green dot won't see it clear until its own next catalog fetch. Matches existing `has_pending_approval` behavior; flagged here so it isn't mistaken for a regression during review.
- **`running_sessions()` cost at large catalog sizes**: one hub-entry-lock acquisition per currently-live session in the workspace, sequential. Fine at this deployment's scale (see Assumptions elsewhere in this doc); would need batching or a cached snapshot if a workspace ever held enough concurrently-live sessions for this to show up in `list_workspaces` latency.

## Implementation Roadmap

- [x] [schema] Add `unseen_outcome` migration on `sessions`, regenerate `schema.rs` via `diesel migration run`
      Purpose: durable storage for the flag
      Verification: migration applies cleanly; `schema.rs` diff is only the new column
- [x] [storage] `WorkspaceStorage::mark_unseen_outcome` / `clear_unseen_outcome`; extend `SessionSummary` and `list_sessions`
      Purpose: read/write path for the flag, independent of the hub
      Verification: `storage_pg` integration tests (`an_unseen_outcome_can_be_marked_and_cleared`, `clearing_an_unseen_outcome_that_was_never_set_is_a_no_op`) against a real PostgreSQL database cover set → list shows it → clear → list shows `None`, and clearing an already-clear row
- [x] [hub] `SessionOpener` trait methods + `AppOpener` impl; `SessionHub.status_tx` + `SessionRelay::subscribe_status`; thread `opener`/`status_tx` through `spawn_event_pipeline`/`run_forwarder`; settle-branch write (skip when `suspended`); `attach()` clear (filtered to rows that actually have something to clear)
      Purpose: core behavior — detect unattended settle, persist, broadcast; clear on attach
      Verification: `hub_tests::unseen_outcome` covers settle-while-unattached sets it (and releases), settle-while-attached does not, suspending-while-unattended does not, and a concurrent attach cannot land between the unattended check and the write (proven by stalling the write behind a gate and asserting the recorded order is always mark-then-clear). Pure classification (`unseen_outcome_for`) unit-tested separately. The invariant the Assumptions section relies on (no path re-enters `turn_running: true` without a prior clearing attach) was verified by code inspection of every caller of `handle_task`/`make_live`'s resuming-agent path, all of which are attach-gated — not by a dedicated regression test, since there was no way to construct one without duplicating that same code-path analysis.
- [x] [server] `AppOpener` new methods delegating to storage; `run_connection` subscribes and forwards `session_status`; wire types
      Purpose: expose the behavior over the JSON-RPC connection
      Verification: `cargo clippy`/`cargo test` across the workspace confirm it compiles and existing wire round-trip tests still pass. The two-tab manual check was **not** performed — no running server/browser session was exercised end-to-end for this step.
- [x] [frontend] `protocol.ts` push + catalog field; `session.ts` handler for the push (updates catalog row + corrects `OpenedSession.running`); the *same* correction applied inside `setCatalog`'s apply path so a `list_workspaces` fetch alone (no push required) also un-sticks a stale `running`; optimistic clear in `openSession`; `status-dot.tsx` new tones; `sidebar.tsx` third indicator branch
      Purpose: user-visible result, with two independent paths to correctness (push, and catalog fetch) rather than one
      Verification: `pnpm --filter coda-web typecheck` + `lint` + `test` all pass, including new unit tests for the extracted pure `reconcileRunningWithUnseenOutcome` (clears a stale `running`, no-ops when there's nothing to clear, and preserves object identity when there's nothing to change — the last of which is what keeps an unaffected row from re-rendering). The manual browser repro (switch away, let it finish, see the dot flip live; reload and confirm it's still correct; open the session and confirm it clears) was **not** performed — no dev server was run for this change.
- [x] [hub] `SessionRelay::running_sessions` + `SessionHub` impl (workspace-scoped scan of live entries' `turn_running`)
      Purpose: expose live running state to the catalog builder, independent of any attachment
      Verification: `hub_tests::unseen_outcome::running_sessions_reports_only_running_sessions_in_the_given_workspace` covers a workspace with one running and one idle session returning only the running one's id, and a same-id session in a *different* workspace being excluded
- [x] [server] `workspace_catalog()` merges `running_sessions()` into `SessionSummaryWire.status` (renamed from `unseen_outcome`), `running` taking priority over the DB-sourced outcome
      Purpose: the actual fix for the refresh bug — the catalog now carries a live signal, not just persisted facts
      Verification: `wire_tests` roundtrip covers `status: "running"` alongside the existing `completed`/`failed`/`null` cases; `cargo clippy`/`cargo test` clean across the workspace. The manual repro (start a task, hard-refresh mid-turn, confirm the yellow dot appears immediately) was **not** performed — no dev server was run.
- [x] [frontend] Renamed `unseen_outcome` → `status` throughout (`protocol.ts`, `session.ts`, `sidebar.tsx`); sidebar's running indicator gains the catalog fallback (`opened?.running ?? session.status === "running"`); `reconcileRunningWithUnseenOutcome` → `reconcileRunningWithStatus` (renamed `clearRunningIfSettled` → `reconcileOpenedSessionRunning`), made symmetric in both directions after `/code-review high` (see Deviations)
      Purpose: user-visible result
      Verification: `pnpm typecheck`/`lint`/`test` all pass, including cases for both correction directions and the `undefined`-vs-`null` distinction. The manual repro (start a task, refresh, confirm yellow appears without waiting for completion) was **not** performed — no dev server was run.

## Deviations from Design

- **`SessionSummary`/`SessionSummaryWire.unseen_outcome` is `Option<String>`, not `Option<UnseenOutcome>`.** The typed enum is used everywhere something branches on it (the write path, the broadcast event); the read path just carries the DB's already-constrained string straight through to JSON, where it needed to become a string again anyway. Nothing downstream ever pattern-matches it as a Rust enum, so parsing it back out of the DB read only to re-serialize it as a string on the way out added a conversion with no reader.
- **Extracted `unseen_outcome_for(&WireEvent) -> UnseenOutcome`** as a small pure function rather than inlining the match in `run_forwarder`, so the Completed/Failed classification has a direct unit test instead of depending on reliably driving a real `Aborted`/`Error` event through the async hub/runtime pipeline in an integration test.
- **Frontend: extracted `reconcileRunningWithStatus`** (originally `reconcileRunningWithUnseenOutcome`, renamed and widened alongside the `status` merge) as an exported pure function (mirroring the existing `applySnapshotToSession`/`reduceEvent` pattern in this file) rather than inlining the correction at its two call sites, for the same testability reason, and because the codebase already has an established convention for it.
- **Test fixtures grew two pieces not anticipated in the brief**: `TestOpener` gained a `mark_unseen_gate`/`mark_unseen_entered` rendezvous pair to make the attach-vs-settle race test deterministic (a first attempt using only relative timing was flaky under `--test-threads=4`, confirmed by repeated runs), and the `TestProvider` gained an `"approval_hold"` prompt so "a suspension settling while genuinely unattended does not mark" could be tested without a race — no other way to hold a suspension open long enough to detach first.
- **`app/coda_server/tests/storage_pg.rs` gained two tests** not called out in the roadmap's storage step, added once it became clear the hub-level tests (which mock `SessionOpener` entirely) never actually exercise the real SQL.
- **`running_sessions()` reads the outer entries map synchronously, then each candidate entry's lock individually, sequentially** — not the batched/cached approach the Risks section flags as a possible future need. Simplest correct version first; revisit only if `list_workspaces` latency actually shows it at a workspace's real session-count scale.
- **`reconcileRunningWithStatus` was one-directional in the first pass, then made symmetric after `/code-review high`.** The original version only corrected a stale `running: true` down to `false` (on a settled `status`), reasoning that forcing it `true` risked overwriting a currently-attached session's fresher live-event value with a momentarily staler catalog read. Review (altitude angle) pointed out this reasoning didn't hold up against a case the design already accepts elsewhere: the `workspace_catalog` DB/hub read race is explicitly tolerated as "a freshness gap, not a correctness one," and the one-directional version left a real, reachable gap — a session opened once in this tab (so it has its own `OpenedSession`), later started running again elsewhere while this tab wasn't attached, stays stuck showing idle through every future reconnect (confirmed reachable via the sidebar's manual "Reconnect" menu item, which doesn't reload the page). Fixed by making the function symmetric: reconcile `running` to match `status === "running"` whenever `status` is a real value (`undefined` — no catalog data for the row yet, e.g. a locally-synthesized "extras" entry — is the only value left alone; an explicit `null` is treated as a confirmed-idle fact, same as a settled outcome).
- **`workspace_catalog()`'s two independent reads (`running_sessions()`, `list_sessions()`) now run via `tokio::join!`** rather than sequentially — a zero-risk latency win caught by `/code-review high`'s efficiency angle, since neither depends on the other's result and the narrow staleness window between them (see Load-Bearing Decisions) is the same regardless of ordering.
- **`openSession`'s optimistic catalog patch now skips itself when the row already reads `"running"`**, instead of unconditionally patching `status: null`. Caught by an external review after the `status` rename: the `status: null` patch predates `status` covering `"running"`, and unconditionally applying it stomps a genuine `"running"` row precisely in the scenario this whole feature exists to fix (a fresh page load, session not yet attached to anything). The window is normally invisible — `requestOpenAndApply`'s snapshot response corrects `OpenedSession.running`, and the sidebar reads `opened.running` over the catalog — but nothing corrects the catalog `status` field itself, so a failed `open_session` request or a dropped connection in that window left it stuck at `null` until the next full catalog fetch or a real settle. Fixed by looking up the row's current status first and only applying the clear when it isn't `"running"`; covered by two new tests in `session-status.test.ts` exercising `openSession` itself (not just the pure `reconcileRunningWithStatus` function).
