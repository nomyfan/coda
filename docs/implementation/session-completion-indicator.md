## Problem

When a background session's turn finishes while the client isn't attached to it (the user switched to another session), the sidebar's running indicator freezes on "running" instead of updating, and there is no way for the user to notice a background session finished at all. Give the sidebar an authoritative, refresh-durable, near-real-time signal for "this session stopped while you weren't watching," and use it to also correct the stale running indicator.

## Scope

In:
- A persisted per-session "unseen outcome" (`completed` / `failed`) recorded when a turn settles with no client attached, covering normal completion, `Aborted`, and `Error`.
- Surfacing it through the existing session catalog (`list_workspaces`) so it's correct after a reload/reconnect.
- A near-real-time push so it appears without a manual refresh, for any connection currently open on the same server process.
- Clearing it (and correcting the stale "running" cache) the moment a client attaches to that session.
- Sidebar rendering: a static green dot for `completed`, static red for `failed`.

Out:
- Suspended-for-approval turns — already covered by the existing `has_pending_approval` catalog field; this mechanism explicitly skips them.
- Per-workspace-scoped delivery — the push is broadcast to every connection on the process, unfiltered, matching how `list_workspaces` already hands every connection the full catalog.
- Propagating the "cleared" state live to *other* tabs attached to a different connection — they pick it up on their next catalog fetch/reconnect, same as `has_pending_approval` today.
- Distinguishing `Aborted` from `Error` visually — both render as `failed`/red.

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

## Components

- **`sessions.unseen_outcome` column** — durable record of "this session ended un-watched," survives restarts and reconnects.
- **`WorkspaceStorage::mark_unseen_outcome` / `clear_unseen_outcome`** — the two writes (diesel `UPDATE ... WHERE (workspace_id, session_id)`), mirroring the existing `rename_session`/`touch` pattern.
- **`SessionOpener::mark_unseen_outcome` / `clear_unseen_outcome`** — the hub's only route to storage; `AppOpener` implements them by delegating to `WorkspaceStorage`, matching how every other hub→storage write already goes through this trait.
- **`SessionHub`'s status broadcast (`status_tx: broadcast::Sender<SessionStatusEvent>`)** — process-wide, best-effort fan-out of "a session's unseen outcome changed," independent of any specific session's attachment.
- **`run_forwarder`'s settle branch** — the single place that already knows, atomically, both "the turn just ended" and "is anyone attached" (`state.attached`); it's where the persist + broadcast fire together.
- **`run_connection`'s new `session_status` push** — forwards a received broadcast event to its client as a JSON-RPC notification, the same way it already forwards per-session `RelayEvent`s.
- **Frontend catalog store slice** — holds `unseenOutcome` per session row; updated by catalog fetch, the new push, and locally on open. **Both** of those two server-driven update paths (catalog apply and the `session_status` push handler) also force the matching `OpenedSession.running` to `false` when the incoming `unseen_outcome` is non-null. This is deliberately redundant: the push is best-effort and may never arrive, but every reconnect/refetch runs `list_workspaces`, so the correction always has a path that doesn't depend on the push. Without this, `sidebar.tsx:605`'s `running={opened?.running ?? false}` has no fallback to the catalog at all (unlike `awaitingApproval`), so a dropped push would leave the exact staleness this design set out to fix.
- **`SessionRow` (sidebar)** — renders it as a third, lowest-priority indicator state.

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
```

```ts
// protocol.ts — new push, sibling to event/snapshot/session_evicted.
type SessionStatusPush = {
  workspace_id: string;
  session_id: string;
  outcome: "completed" | "failed";
};
```

## Data Model

- **`sessions.unseen_outcome`**: nullable text, `check (unseen_outcome in ('completed', 'failed'))`. Lives directly on `sessions` (not a join like `has_pending_approval`'s `thread_checkpoints` EXISTS), so `list_sessions` just selects it — no new subquery.
- **Ownership of the write**: only `run_forwarder` sets it (to a value) and only `attach` clears it (to `NULL`). Both hold the entry's `Arc<Mutex<EntryState>>` guard across the write — see Load-Bearing Decisions. `clear_unseen_outcome`'s `UPDATE` is scoped with `.filter(sessions::unseen_outcome.is_not_null())` — attach is one of the hottest paths and the overwhelming majority of attaches have nothing to clear; Postgres writes a new tuple version on every `UPDATE` regardless of whether the value actually changes, so the filter avoids a wasted dead tuple on the common case.
- **Client-side**: `unseen_outcome` is a field on the catalog's per-session summary row, not on `OpenedSession`. `OpenedSession.running` remains its own thing, but the new push corrects it as a side effect when the two disagree (see Components).

## Load-Bearing Decisions

- **The persist call happens while still holding the entry's async mutex guard**, before the existing `maybe_release` check (`hub.rs:1913-1920`) is even reached. This is a correctness requirement, not a style choice: `attach()` needs that same guard to register a new attachment, so holding it across the write closes the window where a client could attach *between* "we observed nobody's attached" and "we recorded the unseen outcome" — which would otherwise mark a session the user is actively looking at as unseen. The trade-off is a DB round-trip under a lock that other operations on *this session's* entry (not other sessions) must wait out; accepted since it's a single indexed UPDATE on a rare path (once per unattended settle).
- **Broadcast is process-wide and unfiltered**, not scoped per workspace or per subscribed connection. Simpler (no new subscription-management state), consistent with `list_workspaces` already being unscoped, and cheap at this deployment's scale. Revisit if workspaces ever need isolation between clients.
- **`unseen_outcome` is schema-additive** (nullable column, no backfill, no default needed) — consistent with this project's "breaking changes are fine, no compat shims" stance; existing rows just read `NULL`.

## Risks / Open Questions

- **Broadcast channel capacity**: needs a size that comfortably absorbs a burst of settles across many sessions without lagging a slow connection under normal load. Pick a generous default (e.g. 256) and treat lag as acceptable (see Assumptions) rather than tuning precisely up front.
- **Multi-tab staleness on clear**: a second tab showing the same session's green dot won't see it clear until its own next catalog fetch. Matches existing `has_pending_approval` behavior; flagged here so it isn't mistaken for a regression during review.

## Implementation Roadmap

- [ ] [schema] Add `unseen_outcome` migration on `sessions`, regenerate `schema.rs` via `diesel migration run`
      Purpose: durable storage for the flag
      Verification: migration applies cleanly; `schema.rs` diff is only the new column
- [ ] [storage] `WorkspaceStorage::mark_unseen_outcome` / `clear_unseen_outcome`; extend `SessionSummary` and `list_sessions`
      Purpose: read/write path for the flag, independent of the hub
      Verification: storage unit/integration test covers set → list shows it → clear → list shows `None`
- [ ] [hub] `SessionOpener` trait methods + `AppOpener` impl; `SessionHub.status_tx` + `SessionRelay::subscribe_status`; thread `opener`/`status_tx` through `spawn_event_pipeline`/`run_forwarder`; settle-branch write (skip when `suspended`); `attach()` clear (filtered to rows that actually have something to clear)
      Purpose: core behavior — detect unattended settle, persist, broadcast; clear on attach
      Verification: `hub_tests` covers settle-while-unattached sets it, settle-while-attached does not, suspended does not, attach clears it, and the attach-during-settle race does not leave a stale flag. Also add a regression test asserting the invariant the Assumptions section relies on: no path can bring `turn_running` back to `true` for an entry that still has a non-null `unseen_outcome` (i.e. every route into `handle_task`/`make_live`'s resuming-agent case is gated behind an attach that already cleared it).
- [ ] [server] `AppOpener` new methods delegating to storage; `run_connection` subscribes and forwards `session_status`; wire types
      Purpose: expose the behavior over the JSON-RPC connection
      Verification: manual check — two tabs, background a session in tab A, complete its turn via tab B driving something else, confirm tab A gets a `session_status` push without reload
- [ ] [frontend] `protocol.ts` push + catalog field; `session.ts` handler for the push (updates catalog row + corrects `OpenedSession.running`); the *same* correction applied inside `setCatalog`'s apply path so a `list_workspaces` fetch alone (no push required) also un-sticks a stale `running`; optimistic clear in `openSession`; `status-dot.tsx` new tones; `sidebar.tsx` third indicator branch
      Purpose: user-visible result, with two independent paths to correctness (push, and catalog fetch) rather than one
      Verification: `pnpm --filter coda-web lint` + `pnpm --filter coda-web test`; manual repro of the original bug report (switch away from a running session, let it finish, confirm the yellow dot clears and a green dot appears without reload; reload and confirm it's still green; open the session and confirm it clears); additionally simulate a dropped push (e.g. temporarily stub the `session_status` handler as a no-op) and confirm a reconnect/`list_workspaces` refetch alone still clears the stale yellow dot
