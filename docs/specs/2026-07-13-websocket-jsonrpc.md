# Design Brief: JSON-RPC over the single WebSocket

Date: 2026-07-13
Status: Proposed (rev. 5 — corrects: delete-on-disconnect settled by an idempotent re-`delete_session` on reconnect (the catalog-absence settle was unsound — catalog reflects only persisted sessions and folds read errors to empty), notifications never produce a reply even on unknown-method/invalid-params, Decision 11 wording; rev. 4 — delete-on-disconnect tombstone, `INVALID_MODEL_SELECTION` attributed to pre-hub validation, every-task gate wording; rev. 3 — in-flight `deleting` guard for deferred delete, best-effort allow-pattern that never blocks `resume`, `notify(): boolean` gate generalized to *every* task, `add_allow_pattern` unknown-workspace error; rev. 2 — frozen numeric error codes + client `JSONRPCErrorException` mapping, `set_model` `Ignored` covers the pre-dispatch guard path, `notify(): boolean` gating the optimistic append; rev. 1 — parse-error boundary, hub `set_model` outcomes, delete failure contract, draft `open→task` ordering)

## Problem

The client/server wire has only fire-and-forget commands and server pushes; the UI needs true request→response calls (list catalogs, open session, set model, add allow-pattern) but must correlate replies by message *type* + workspace/session id, which is fragile and gives no uniform error channel.

## Scope

**In:**
- Replace the flat `ClientMessage`/`ServerMessage` framing with a JSON-RPC 2.0 envelope carried over the **existing single WebSocket** (`/ws`).
- Request→response (id-correlated `result`/`error`) for the naturally synchronous calls.
- Notifications (no id) for fire-and-forget commands and all server-initiated pushes, including the streaming turn.
- A promise-based client built on the `json-rpc-2.0` library (id allocation, pending map, and response correlation owned by the library — we write no id-mapping code), replacing the manual `onmessage` type-switch. The server keeps a small hand-written envelope module; there is no equivalent transport-agnostic Rust lib that also does server-push without owning the socket (see Alternatives).
- A uniform error object (standard + a small app-specific code set) so failures that today are silently `warn!`-and-dropped become real responses.

**Out:**
- No separate HTTP endpoint. (Explicitly rejected — see Alternatives.)
- No JSON-RPC batching (arrays).
- No server→client *requests* — the server only responds and pushes. (`ask_user` keeps using the existing suspend/resume flow.)
- No backward-compat/dual-protocol shim. Per project policy this is a clean cutover: Rust and TS wire change together.
- No change to the agent runtime, storage, or approval logic. **The hub (`SessionHub`) is touched in exactly one place:** `command`'s `CommandOutcome` gains finer variants so `set_model` can return a precise result/error (see Decision 8). `RelayEvent`, the attach/detach/release flow, and every other command path are untouched — the outcome split is the state owner absorbing complexity the dispatcher can't.

## Assumptions

- **`coda_web` is the only wire client.** A single known client means we can hard-cutover and skip full 2.0 conformance edge cases (batching, server-side requests). If a second client (CLI, other) is reintroduced, the envelope is standard enough to interoperate.
- The one live `Session` per connection / latest-wins model in the hub stays. All request handlers already run inside `run_connection`, which owns the connection's `streams`/`selections` — so requests keep first-class access to session subscription state without any cross-connection lookup.
- MCP (already integrated, itself JSON-RPC 2.0) is the consistency target; we mirror its envelope conventions where cheap.

## Validation Findings

Code reading was the probe (no runtime experiment needed for a wire redesign).

- **Q: Which current messages are really request/response vs. push?** Read [`handle_connection_command`](../../app/coda_server/src/bin/server.rs). Result: `list_workspaces`, `list_providers`, `open_session`, `set_model`, `add_allow_pattern`, `delete_session` each have an identifiable *synchronous reply path*, but the reply count and failure semantics are **not uniform** — e.g. a successful `open_session` sends *two* messages (`Snapshot` then `WorkspaceCatalog`), while several failure paths (`set_model` invalid, unknown workspace) send *nothing*; `task`, `resume`, `abort`, `close_session` route into the hub and reply only via the event stream. Implication: the request/notification split maps onto existing handlers, but converting them means collapsing each to *exactly one* framed result-or-error (dropping the follow-up `WorkspaceCatalog` push on open; see the cleanup step) and adding the missing failure replies — it is reframing plus a settle-exactly-once normalization, not a pure 1:1 rename.
- **Q: Is any reply payload emitted both solicited and unsolicited?** Yes. `WorkspaceCatalog`/`ProviderCatalog` are pushed at connect (server.rs ~961–964) *and* returned to `list_*`; `Snapshot` is returned to `open_session` *and* pushed unsolicited on hub re-attach ([`RelayEvent::Closed` → `attach_and_stream`](../../app/coda_server/src/bin/server.rs)). Implication: the *payload* types must be reusable as both a `result` and a notification `params`; only the envelope differs. The shared store reducers (`applySnapshot`, `setCatalog`, `setProviderCatalog`) stay; only their plumbing forks.
- **Q: Blast radius on the Rust side?** `grep` for `ClientMessage`/`ServerMessage`: the envelope lives only in `wire.rs`, `transport.rs`, `bin/server.rs`. The hub emits `RelayEvent(WireEvent)` and never touches `ServerMessage`. Implication: framing change is isolated; `WireEvent` and all `*Wire` payload structs are untouched.
- **Q: Do any requests currently drop silently?** Yes — invalid `set_model`, unknown-workspace `open_session`, invalid session id all `warn!` and `return true`, sending the client nothing. Implication: once these are requests, "every request yields exactly one response" forces them to become explicit errors (a correctness improvement, and required to avoid leaking client promises).
- **Q: Can the current `Transport` even surface a malformed frame to the rpc layer?** No. [`decode`](../../app/coda_server/src/transport.rs) `warn!`s and returns `None`, and `recv` loops past it — a bad frame is silently skipped, so the rpc layer never sees it. Implication: for the rpc layer to answer a parse/invalid-request error, `Transport::recv` must hand up the **raw frame text** and let the rpc layer classify. Also, per the spec, invalid JSON is `-32700` (parse error), not `-32600` (that's valid-JSON-but-invalid-Request) — the earlier draft conflated them.
- **Q: Does the hub give `set_model` a single unambiguous outcome?** No, and the ambiguity spans two functions. `SessionHub::command` returns `Ignored` at its `lock_entry_for_conn` guard — *before* dispatch — for a stale/not-attached conn or missing entry ([hub.rs:875](../../app/coda_server/src/hub.rs)); [`handle_set_model`](../../app/coda_server/src/hub.rs) then returns `Ignored` again for a non-`Live` phase, an identical model (benign no-op), and a turn in flight (soft reject), plus `ModelChanged`/`OpenFailed`. Implication: the fix can't live solely in `handle_set_model` (the guard path skips it); the two non-error cases are peeled into their own outcomes and the dispatcher reads residual `Ignored` as `SESSION_NOT_LIVE` (Decision 8).
- **Q: What does `delete_session` do when deletion actually fails?** [The handler](../../app/coda_server/src/bin/server.rs) rejects (sends nothing) when `relay.delete` returns `false` (another connection is attached), and on a storage-delete error it only `warn!`s and **still sends the catalog** — the client reads that as success. Implication: `delete_session` needs a real failure contract and must return the catalog only after a durable delete (Decision 9).

## Alternatives Considered

- **Second HTTP endpoint for request/response** (rejected, per user). The server keys one live `Session` to the WebSocket connection (latest-wins single client). An HTTP request would have to reach into that per-connection session, forcing a connection↔session registry and a second auth/lifecycle path. Running requests over the same WS keeps session ownership trivial and needs no new transport.
- **Minimal "add an `id` field to the existing enums"** instead of a real envelope. Cheaper, but reinvents half of JSON-RPC (error object, notification vs. request marker) ad hoc and diverges from the MCP style already in the tree. Full 2.0 envelope costs a little more now and buys consistency + a spec to point at.
- **One flatten + adjacently-tagged enum** (`#[serde(tag="method", content="params", flatten)]` inside an envelope struct with `id`) for typed one-shot decode. Elegant but serde's `flatten` + tagged-enum path has known buffering limitations and is a latent footgun. Chosen instead: envelope decodes `method: String` + `params: serde_json::Value`, then the dispatcher `from_value`s params into the per-method type. Fully robust, zero serde magic, and how most hand-rolled JSON-RPC servers do it.
- **`jsonrpsee` (full Rust framework) owning the connection** (rejected). It builds every envelope for you — which matches the "don't hand-write protocol bodies" goal on the *server* — but it takes over the WS upgrade and the per-connection message loop, so the hub's per-connection identity (`conn_id`, latest-wins eviction, re-attach on `Closed`) has to be re-expressed on top of its connection context. Worse, its only server→client push mechanism is *subscriptions*, a jsonrpsee-specific wire convention with no browser/JS client — so the TS side would hand-write glue to match it anyway. Net: heavy coupling to the framework plus a bespoke wire the client can't consume for free.
- **Hand-rolling the client correlation too** (rejected in favor of `json-rpc-2.0`). A ~100-line `RpcClient` would give maximal control, but `json-rpc-2.0` is transport-agnostic (we hand it a send function; it never touches the WebSocket), tiny, and removes exactly the id/pending-map code we don't want to own. The bespoke parts we *do* keep (reconnect, eviction) sit above it via `rejectAllPendingRequests`.
- **Keep open/set-model/etc. as notifications with typed push replies.** Would preserve the current shape but keep correlation implicit. Making them requests lets the client `await` and lets failures be typed `error`s at the call site — the whole point of the change.

## Components

- **`rpc` (server, new `app/coda_server/src/rpc.rs`)** — JSON-RPC envelope: decode a **raw frame string** into request / notification / invalid (distinguishing `-32700` parse error from `-32600` invalid request, recovering the `id` when the JSON is structurally valid); build outgoing `result`/`error`/notification frames; the app error-code set. Domain-agnostic; deals in `serde_json::Value` at its seam.
- **`wire` (server, existing, reshaped)** — Per-method **params** and **result** payload structs (most already exist as `*Wire`), plus the server→client **notification** payloads (`event`, `session_evicted`, `snapshot`, `workspace_catalog`, `provider_catalog`). No top-level `ClientMessage`/`ServerMessage` enums anymore.
- **`transport` (server, existing)** — Now a plain text-frame mover: `recv` yields the **raw frame string** (decoding + malformed-JSON classification move up to `rpc`, which is the only layer that can turn a bad frame into an error response); `send` takes the typed `RpcOutgoing` and serializes it. The decode/encode asymmetry is intentional — decode must distinguish parse-vs-structure failures, encode cannot fail structurally.
- **Connection dispatcher (`bin/server.rs`, existing `handle_connection_command`)** — Match on `method`, deserialize params, run the handler, frame the reply with the request id (or as an error); notifications run for effect. `run_connection`'s stream arm frames hub `RelayEvent`s as notifications.
- **`rpc client` (web, new `app/coda_web/src/lib/rpc.ts`)** — A thin adapter over `json-rpc-2.0`'s `JSONRPCServerAndClient`: supply the "send this JSON over the WebSocket" function, feed inbound frames to `receiveAndSend`, and register push handlers via `addMethod`. The library owns id allocation, the pending map, and response correlation; on socket close we call `rejectAllPendingRequests` so no await hangs. No id-mapping code here.
- **`session store` (web, existing `session.ts`, reshaped)** — Request actions `await rpc.request(...)`, apply the result at the call site, and `catch` a `JSONRPCErrorException` to branch on `.code` (e.g. `SESSION_BUSY` → `applyHeldElsewhere`); fire-and-forget actions call `rpc.notify(...)` and, where an optimistic update follows (**every** task — new or existing session), append only when it returns `true`; the shrunken push router feeds the existing reducers.

## Interfaces

Server envelope (`rpc.rs`):

```rust
/// Classify one raw inbound frame. `params` stays a Value; the dispatcher
/// deserializes it per method. `Invalid` is either a `-32700` parse error
/// (the frame wasn't JSON: `id` is None → answered with id `null`) or a
/// `-32600` invalid request (JSON parsed but isn't a well-formed call: `id`
/// recovered when present). Answered, never dropped.
fn decode(frame: &str) -> Incoming;
enum Incoming {
    Request { id: RpcId, method: String, params: Value },
    Notification { method: String, params: Value },
    Invalid { id: Option<RpcId>, error: RpcError },
}

/// Frame a successful result / a failure / a server push. Each returns the
/// value the transport serializes; `id` echoes the request's id verbatim
/// (or `null` when a parse error left no id to echo).
fn result(id: RpcId, payload: &impl Serialize) -> RpcOutgoing;
fn error(id: Option<RpcId>, err: RpcError) -> RpcOutgoing;
fn notify(method: &str, params: &impl Serialize) -> RpcOutgoing;

struct RpcError { code: i32, message: String, data: Option<Value> }
```

**Frozen error codes** (the wire carries only the number; both ends mirror this table — `rpc.rs` constants ↔ `protocol.ts` `RpcCode`). App codes sit in the JSON-RPC-reserved implementation-defined server-error block (`-32000..-32099`):

| Code | Name | Raised by | Client handling |
|---|---|---|---|
| -32700 | PARSE_ERROR | non-JSON frame | log; connection-level |
| -32600 | INVALID_REQUEST | JSON but not a valid call | log; connection-level |
| -32601 | METHOD_NOT_FOUND | unknown `method` | log |
| -32602 | INVALID_PARAMS | `params` fails `from_value` | log |
| -32603 | INTERNAL_ERROR | unexpected server fault | surface as error activity |
| -32001 | SESSION_BUSY | `open_session` (another client holds it) | **`applyHeldElsewhere(…, "busy")`** — drives the takeover UI |
| -32002 | NOT_OWNER | `delete_session` (`relay.delete` refused) | keep session in list; error toast |
| -32003 | SESSION_NOT_LIVE | `set_model` (stale/not-attached/not-Live) | revert model selector |
| -32004 | MODEL_SWITCH_WHILE_RUNNING | `set_model` (turn in flight) | revert selector; hint "can't switch mid-turn" |
| -32010 | UNKNOWN_WORKSPACE | `open_session`/`delete_session`/`add_allow_pattern` | error activity |
| -32011 | INVALID_SESSION_ID | `open_session`/`delete_session` | error activity |
| -32012 | INVALID_MODEL_SELECTION | `set_model` | revert selector |
| -32020 | OPEN_FAILED | `open_session`/`set_model` promotion | error activity |
| -32021 | DELETE_FAILED | `delete_session` (storage error) | keep session in list; error toast |
| -32030 | ALLOW_PATTERN_FAILED | `add_allow_pattern` | error activity (message in `data`) |

On the client, `json-rpc-2.0` rejects a `request(...)` promise with a **`JSONRPCErrorException`** carrying `.code`, `.message`, `.data` — *not* the raw Rust `RpcError`. Call sites `catch` and branch on `.code` against `RpcCode`; the mapping above is the contract (notably `SESSION_BUSY` must reach `applyHeldElsewhere` or the takeover flow regresses).

**Dispatch rule — who gets a reply.** A *request* (has `id`) always produces exactly one framed reply: a `result`, or an error — unknown `method` → `-32601`, `params` that fail `from_value` → `-32602`. A *notification* (no `id`) produces **no reply, ever** — an unknown method or invalid params on a notification is logged and dropped, because there is no `id` to answer against; emitting an error for it would violate JSON-RPC. Only a frame that isn't JSON (`-32700`) or isn't a structurally valid call at all (`-32600`) is answered with `id: null`; a well-formed notification never lands in that bucket. This is why the dispatcher must classify request-vs-notification *before* attempting `from_value` on `params`.

Client (`rpc.ts`) — a thin **adapter** over `json-rpc-2.0`, not the raw library, because two behaviors need shaping:

```ts
// Wraps client.request. id + pending map + correlation live inside the library.
// Resolves with the method's typed result, or REJECTS with a
// JSONRPCErrorException ({ code, message, data }) — call sites branch on .code
// against RpcCode (see the error table).
request(method, params): Promise<Result>;

// Wraps client.notify, but returns a boolean the store can gate on. The library's
// own notify() is void and swallows send failures, so the adapter FIRST checks the
// socket exists and readyState === OPEN: false → don't send, return false; else
// call the library and return true. Fire-and-forget past that point.
notify(method, params): boolean;

// Register a server-push handler (event / session_evicted / snapshot / …); feeds
// the existing store reducers. Same reducer whether solicited or pushed.
addMethod(method, (params) => void): void;

// On socket close, so no awaiting caller hangs:
rejectAllPendingRequests(reason): void;
```

**Task ordering contract (Decision 10) — two rules:** (1) **Every** task (new *or* existing session) appends the optimistic user message and sets `running` **only if `notify("task")` returned `true`**; a `false` (socket absent/closed) surfaces "disconnected" and does not append. (2) A **draft/new session additionally** `await`s `request("open_session", …)` and requires success *before* that `notify` — an open rejection surfaces its error and sends no task. So the UI never shows a message + spinner for a turn that never left the client, on either a failed open or a dead socket.

## Data Model

**Method catalog** (method names reuse today's snake_case `type` tags):

Direction is `C→S` (client→server) or `S→C` (server→client). "Kind" is the JSON-RPC message shape: a **request** carries an `id` and gets a correlated response; a **notification** carries no `id` and gets no reply. Note both kinds occur in both directions — a `notification` is not inherently server-side.

| Method | Direction | Kind | Result / effect |
|---|---|---|---|
| `list_workspaces` | C→S | request | `{ workspaces }` |
| `list_providers` | C→S | request | `{ providers, default_provider }` |
| `open_session` | C→S | request | `Snapshot` payload; error `SESSION_BUSY` / `OPEN_FAILED` / `UNKNOWN_WORKSPACE` / `INVALID_SESSION_ID`. Side effect: subscribes this connection's event stream. |
| `set_model` | C→S | request | `{ provider_id, reasoning_effort }` on a real switch **and** on a no-op where the model is already selected (idempotent success, echoing the current selection); errors: `INVALID_MODEL_SELECTION`, `OPEN_FAILED`, `MODEL_SWITCH_WHILE_RUNNING` (turn in flight), `SESSION_NOT_LIVE` (stale/not attached). Requires the hub outcome split (Decision 8). |
| `add_allow_pattern` | C→S | request | `{}` on success; errors `UNKNOWN_WORKSPACE` (was a silent `return true`) and `ALLOW_PATTERN_FAILED` (write failed; message in `data`). Replaces the old `error: Option<String>` field and pattern-based correlation. |
| `delete_session` | C→S | request | `{ workspaces }` — returned **only after** a durable delete; errors: `INVALID_SESSION_ID`, `UNKNOWN_WORKSPACE`, `NOT_OWNER` (another connection holds the session; `relay.delete` refused), `DELETE_FAILED` (storage removal errored). Client removes the local session only on success (Decision 9). |
| `task` | C→S | notification | no reply; effects stream back as `event` notifications (S→C) |
| `resume` | C→S | notification | no reply; further `event` (Suspended) / error `event`s stream back (S→C) |
| `abort` | C→S | notification | no reply; observable as an `aborted` `event` (S→C) |
| `close_session` | C→S | notification | no reply; server-side lifecycle release |
| `event` | S→C | notification | one `WireEvent` (unchanged shape) |
| `session_evicted` | S→C | notification | `{ workspace_id, session_id }` |
| `snapshot` | S→C | notification | `Snapshot` payload (unsolicited hub re-attach only) |
| `workspace_catalog` | S→C | notification | `{ workspaces }` (mutation reconciliation, if kept — see Decisions) |
| `provider_catalog` | S→C | notification | `{ providers, default_provider }` (only if any unsolicited push remains) |

**Ownership / shared state:** the connection dispatcher owns `streams` and `selections` (per-connection). Request ids are client-owned (allocated by `json-rpc-2.0`, opaque to the server, echoed verbatim). The pending-request map lives inside the `json-rpc-2.0` client instance — not in our code. No new shared mutable state **on the server** (the concurrency-sensitive side). The client store gains one new per-session field, `deleting` (Decision 9) — ordinary single-threaded store state, not shared/concurrent.

## Load-Bearing Decisions

1. **JSON-RPC 2.0 envelope over the single WS; no HTTP.** Trade-off: a wire break (acceptable per policy) for correlation, a uniform error object, and MCP-consistency — without a second transport or session registry.
2. **Every request yields exactly one response (result *or* error); no silent drops.** Trade-off: handlers that today `warn!`+ignore must now produce typed errors. This is more code but is the correctness backbone — a dropped reply is a leaked client promise.
3. **`params`/`result` are `Value` at the envelope seam; typed at the handler.** Deep envelope module, typed handlers, robust decode. Trade-off: one `from_value` per dispatch arm vs. relying on fragile serde flatten+tag.
4. **Asymmetric roles.** Client sends requests+notifications; server sends responses+notifications only. Trade-off: `ask_user` can't become a server→client request later without extending the layer — but it works today via suspend/resume, so no cost now.
5. **Payload structs are shared across solicited/unsolicited paths.** `Snapshot`, catalogs serialize identically whether framed as a `result` or a notification `params`; the store reducer is shared, only the routing forks. Trade-off: none significant; it's the natural consequence of (1).
6. **`open_session` is a request that also subscribes.** Its `result` is the snapshot; subsequent events are `event` notifications on the same connection. The re-attach path pushes an unsolicited `snapshot` notification using the same payload. Requires refactoring `attach_and_stream` to *return* the snapshot payload (caller frames it as result or notification) rather than sending it itself.
7. **`json-rpc-2.0` on the client; a one-time hand-written envelope on the server; `jsonrpsee` not adopted.** The client writes zero id/correlation code — the library owns it. The server's envelope module (`result`/`error`/`notify` framers + decode) is the *only* hand-written protocol code, and it's written once; feature-code handlers never build a JSON-RPC body. Trade-off: no free lunch — no transport-agnostic Rust lib does server-push without owning the socket, so the server keeps ~150 lines of envelope; in exchange the hub loop stays untouched and the event stream rides *standard* notifications (not jsonrpsee's bespoke subscription wire, which the browser can't consume for free).
8. **`set_model` gets a precise contract by peeling the *non*-error cases off `Ignored`, then reading residual `Ignored` as `SESSION_NOT_LIVE`.** The subtlety the review caught: `Ignored` is produced in *two* places for a `set_model`, and one is unreachable from `handle_set_model`. `SessionHub::command` returns `Ignored` at its `lock_entry_for_conn` guard — *before* the `match` — for a stale/not-attached conn or missing entry ([hub.rs](../../app/coda_server/src/hub.rs) `command`); `handle_set_model` also returns `Ignored` when the entry isn't in `Live` phase. Both genuinely mean "not live / stale". So the fix is **not** to make `handle_set_model` emit a `NotLive` (the guard path never reaches it); instead:
   - Split `handle_set_model`'s two *non*-error `Ignored` returns into distinct outcomes: `Unchanged` (model already selected → idempotent **success**) and `TurnRunning` (→ `MODEL_SWITCH_WHILE_RUNNING`). Leave its not-`Live`-phase branch returning `Ignored`.
   - An invalid model/effort is caught **before** the hub, by `normalize_provider_selection` in the handler → `INVALID_MODEL_SELECTION` (it never reaches `command`; `OpenError` has no "bad model" variant — only missing-field / storage / pending-approvals). The dispatcher's `set_model` arm then maps the hub outcomes: `ModelChanged`/`Unchanged` → success, `TurnRunning` → `MODEL_SWITCH_WHILE_RUNNING`, `OpenFailed` → `OPEN_FAILED`, and **any residual `Ignored` → `SESSION_NOT_LIVE`**. This is sound because, once the two success/soft-reject cases are peeled off, every remaining `Ignored` on the `set_model` path (guard *or* non-Live phase) is exactly "not live/stale".

   Trade-off: `command`'s generic guard stays untouched (no per-command awareness there); the interpretation lives in the one request arm that consumes it. Notifications (`task`/`resume`/`abort`/`close_session`) keep ignoring `Ignored` — only the request path reads it as an error.
9. **`delete_session` returns the catalog only after a durable delete; the client marks the session `deleting` while the request is in flight and only removes it on success.** The refusal path (`relay.delete` false → `NOT_OWNER`) and the storage-error path (→ `DELETE_FAILED`, no catalog) both become typed errors instead of a silent success. Awaiting the reply opens a window the old optimistic removal didn't have: for that round-trip the session still exists in the store, so without a guard the user could `task`/`set_model`/re-`open` it — and since the server may have *already* deleted it, those frames get dropped, or an `open` **resurrects** the just-deleted id as a fresh empty session. So the store gains a per-session `deleting` flag: while set, `open_session`, `task`, `set_model`, and a repeat `delete` for that key are no-ops. Settling it depends on *how* the request ends:
    - **Success response** → remove the session.
    - **Explicit error** (`NOT_OWNER`/`DELETE_FAILED`/`UNKNOWN_WORKSPACE`/`INVALID_SESSION_ID`) → the delete definitively did *not* commit; clear the flag, session returns to normal.
    - **Socket close (ambiguous)** → a dropped connection does **not** prove the delete didn't commit; the server may have deleted durably and lost the response. So the flag is *kept as a tombstone across the disconnect*, and three things follow: (1) `activeSessionToRestore` must **not** auto-`open_session` a tombstoned session on reconnect — that is exactly the resurrection vector (server re-creates the id as a fresh empty session because the checkpoint dir is gone); (2) `mergeCatalog` must **not** re-add a tombstoned session as an "extra" (it currently re-adds local sessions absent from the incoming catalog); (3) on reconnect the client **re-sends `delete_session`** and settles on its explicit result/error. This is safe because delete is already idempotent: `SessionHub::delete` returns success when nothing is live ([hub.rs:954](../../app/coda_server/src/hub.rs)) and `WorkspaceStorage::delete_session` treats a missing directory as success ([storage.rs:66](../../app/coda_server/src/storage.rs)). So a re-delete of an already-deleted id resolves **success** (remove locally, drop the tombstone), whereas one that now hits a live session another client holds resolves **`NOT_OWNER`** (clear the tombstone — it was never deleted, the user can act again).

    Considered and rejected: (a) optimistic removal + a saved snapshot for rollback — re-introduces the phantom-deletion flash and makes failure restore list position / active-selection state; (b) settling from the reconnect `list_workspaces` catalog — **unsound**: the catalog reflects only *persisted* sessions and folds a `list_sessions` read error into an empty list ([`workspace_catalog`](../../app/coda_server/src/bin/server.rs:375) uses `Vec::new()` on error), so an "absent" id conflates *deleted*, *list-read-failed*, and *unpersisted live session held by another client* — it cannot prove the delete committed. The idempotent re-delete gives a definitive answer the catalog can't. Trade-off: a tombstoned session shows a disabled/"deleting" state until the reconnect re-delete settles it, in exchange for no phantom deletion and, crucially, no resurrection.
10. **Every task gates its optimistic append on `notify("task") === true`; a draft/new session additionally awaits `open_session` first.** Two separate rules, not one:
    - **All tasks** (new *and* existing live sessions) — append the user message and set `running` **only if** the adapter's `notify` returned `true` (socket present and `OPEN`). The library's own `notify()` is `void` and swallows send failures, so without the boolean the UI could show a message + spinner for a turn that never left the client.
    - **Draft/new session only** — additionally `await request("open_session", …)` and require it to succeed *before* the `notify("task", …)`; a fire-and-forget task sent before the session is live would be silently dropped server-side while the UI already showed it running.

    Trade-off: a new session waits one open round-trip before streaming (imperceptible); every task pays a cheap `readyState` check. Removes the stuck-`running` failure mode on both a failed open and a dead socket.
11. **Approval submit: allow-pattern persistence is best-effort and never blocks `resume`.** `submitApprovals` does two independent things per approval — persist any "always allow" patterns (`add_allow_pattern`, now a *request*) and continue the turn (`resume`, a *notification*). They must not be coupled: a rejected allow-pattern must not strand an already-approved tool call. Contract: fire the allow-pattern requests and gather them with `Promise.allSettled` (a rejection only logs a non-fatal allow-pattern error activity — `ALLOW_PATTERN_FAILED` or `UNKNOWN_WORKSPACE` — never throws out of the submit); then send `resume`; clear the approval + allow drafts **only if** `notify("resume")` returned `true`, so a resume that never reached the socket (disconnect) leaves the draft intact for retry. Trade-off: an approval whose allow-pattern write failed still resumes (correct — the tool was approved) and the user sees a non-fatal "couldn't save always-allow" note; the pattern simply isn't persisted for future turns.

## Risks / Open Questions

The three review-surfaced contract gaps (error boundary, hub outcome, delete semantics) are settled above and must land as specified; `snapshot` parity is one of several top risks, not the sole one.

- **`snapshot` dual path parity.** The unsolicited `snapshot` notification (hub re-attach) must reproduce today's `applySnapshot(..., replaceEmpty)` semantics that the connect closure currently computes from `sessionToRestore?.key`. Getting this subtly wrong regresses reconnect/restore. *Find out early:* build the `open_session` request + `snapshot` notification first and exercise reconnect-mid-turn and takeover before touching anything else.
- **Every request settles exactly once (error boundary).** With handlers now producing typed errors, the audit is: does *each* request arm — including the newly-typed `set_model`/`delete_session` failure paths and the `Invalid` parse/invalid-request path — emit one and only one framed reply? A missed arm hangs the client promise; a double-send corrupts correlation. *Find out early:* a server-side test that every method+error path yields exactly one `RpcOutgoing` with the request's id (or `null` for parse errors).
- **Catalog reconciliation after open/delete.** Today the server pushes `workspace_catalog` after `open_session` and `delete_session`. Proposed: `delete_session`'s *result* carries the fresh catalog, and the post-`open` push is dropped (the client already optimistically inserts via `upsertCatalogTitled`, and a later `list_workspaces` reconciles). *Open:* confirm no list flicker/regression when a brand-new session is first opened; if there is, keep a `workspace_catalog` notification after open.
- **Connect-time eager pushes dropped.** The server currently pushes both catalogs at connect *and* the client requests them in `onopen` (duplication). Proposed: drop the eager push; the client's two `request`s are the sole source. *Verify:* first paint still populates sidebar + model selector.
- **Transport-liveness propagation.** Today a failed `transport.send` returns `false` and breaks `run_connection`. The dispatcher's reply-send and any mid-handler notification-send must preserve that "socket dead → stop" signal.

## Implementation Roadmap

- [ ] **[risk validation] `open_session` request + `snapshot` notification, end to end.**
      Build the server envelope (`rpc.rs`), wire `json-rpc-2.0` (`JSONRPCServerAndClient`) to the socket on the client (send fn + `receiveAndSend`), and convert only `open_session` (request, result=snapshot, `SESSION_BUSY` error) and the unsolicited `snapshot` notification (via `addMethod`). Refactor `attach_and_stream` to return the snapshot payload.
      Purpose: validates the riskiest assumption (snapshot dual-path parity + subscribe-on-request) before the rest depends on it.
      Verification: open a session, reconnect mid-turn, and take over from a second tab — transcript, approvals, and running state match `main`.

- [ ] **[core envelope] Freeze the envelope + error codes and the dispatcher shape.**
      `rpc::decode(&str)` classifying request / notification / invalid, `result`/`error`/`notify` framers, the app error-code enum, and the `method`-string dispatch in `handle_connection_command`; `Transport::recv` switches to yielding the raw frame string.
      Purpose: locks the contract every remaining method plugs into, and makes the parse/invalid-request boundary real.
      Verification: `cargo test` round-trips for envelope decode/encode and each error code; a non-JSON frame yields `-32700` with id `null`, a structurally-invalid request `-32600`, and an unknown-method *request* `-32601`; and — asserting the notification rule — an unknown-method *notification* and an invalid-params *notification* each yield **no** `RpcOutgoing` (log-and-drop), while the same payloads as requests yield `-32601`/`-32602`.

- [ ] **[hub outcome] Peel `Unchanged` / `TurnRunning` off `handle_set_model`; map residual `Ignored` → `SESSION_NOT_LIVE` in the dispatcher (Decision 8).**
      Add `CommandOutcome::Unchanged` and `TurnRunning`; `handle_set_model` returns them for the same-model and turn-running cases and keeps `Ignored` for the non-`Live` phase. The `command` pre-dispatch guard is left as-is. Invalid selections are rejected by `normalize_provider_selection` *before* the hub call → `INVALID_MODEL_SELECTION`. The dispatcher's `set_model` arm maps the hub outcomes `ModelChanged`/`Unchanged` → success, `TurnRunning` → `MODEL_SWITCH_WHILE_RUNNING`, `OpenFailed` → `OPEN_FAILED`, residual `Ignored` → `SESSION_NOT_LIVE`.
      Purpose: gives `set_model` a reply the dispatcher can produce without guessing — including the stale/not-attached path that never reaches `handle_set_model`.
      Verification: hub unit tests for same-model → `Unchanged`, mid-turn → `TurnRunning`; a dispatcher test that a `set_model` from a non-attached conn (guard `Ignored`) resolves as `SESSION_NOT_LIVE`, not success.

- [ ] **[remaining requests] Convert `list_workspaces`, `list_providers`, `set_model`, `add_allow_pattern`, `delete_session`.**
      Each becomes a request with a typed result; failures become typed errors (drop the `allow_pattern_result.error` field and the pattern-correlation entirely). `add_allow_pattern` returns `UNKNOWN_WORKSPACE` instead of the old silent `return true`. `delete_session` returns the catalog only after a durable delete and errors otherwise (`NOT_OWNER`/`DELETE_FAILED`/…); the store marks the session `deleting` while the request is in flight — no-op'ing `open`/`task`/`set_model`/repeat-`delete` for it — removes it on success, clears the flag on an explicit error, and on socket close keeps it as a tombstone that suppresses auto-restore (`activeSessionToRestore`) and mergeCatalog re-add, then on reconnect re-sends the (idempotent) `delete_session` and settles on its result/error (success ⇒ remove, `NOT_OWNER` ⇒ clear) (Decision 9).
      Purpose: delivers the actual request/response UX; removes implicit correlation; closes the phantom-delete and resurrection gaps.
      Verification: client actions `await` and update state at the call site; invalid `set_model` rejects instead of hanging; a delete refused as `NOT_OWNER` leaves the session in the list; a session mid-delete can't be re-opened or tasked; a delete whose response is dropped by a forced disconnect does not resurrect on reconnect (settled by the idempotent re-delete, not by catalog absence).

- [ ] **[notifications] Convert `task`, `resume`, `abort`, `close_session` to the adapter's `notify`; `event`/`session_evicted` to server notifications; replace the client `onmessage` type-switch with `addMethod` push handlers.**
      *Every* task appends + sets `running` only when `notify("task")` returns `true`; a draft/new session additionally `await`s its `open_session` result first (Decision 10). `submitApprovals` becomes `Promise.allSettled` over the best-effort `add_allow_pattern` requests (rejections only log activity), then `notify("resume")`, clearing the approval/allow drafts only when that returns `true` (Decision 11).
      Purpose: completes the split; the streaming turn rides notifications tied to no id, without the stuck-`running` window and without a failed allow-pattern stranding an approved call.
      Verification: a full turn streams (llm chunks, tool start/end, suspend→resume, abort) identically to `main`; a new session whose open is forced to fail shows the error and never a hung spinner; a rejected allow-pattern still resumes the approved call; `pnpm --filter coda-web lint` clean.

- [ ] **[cleanup] Drop connect-time eager catalog pushes; decide the post-open/delete catalog path per the Risks section; delete dead `ClientMessage`/`ServerMessage` enums.**
      Purpose: removes the redundancy the old shape carried.
      Verification: first paint populates sidebar + model selector from the client's own requests; `cargo clippy` + `cargo test` + oxlint all clean.
