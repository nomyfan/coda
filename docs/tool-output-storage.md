# Tool output storage

Tool collection memory, retained files and model-visible responses have separate limits. Defaults are 256 KiB of capture memory per active output, 64 MiB of saved payload per result, 512 MiB per session and 4 GiB across the service. Model responses default to 16 KiB per tool and 64 KiB per batch, including the preview, paths and metadata. These sizes are UTF-8 bytes, not tokens.

Configure `[resources.output]`, `[resources.output.model]` and `[resources.ptc]` in the server TOML; the [example configuration](../examples/coda-server.toml) lists every setting. Provider model entries can override `output_limits.single_bytes` and `output_limits.batch_bytes` independently. Changes require a restart. Startup rejects incompatible limits and response budgets too small to include complete file paths. Capture may spill before exhausting its memory allowance to reserve space for preview rendering, UTF-8 decoding and queued IO.

## Deployment and retention

Set `resources.output.root` to a stable absolute path on persistent local storage. Relative paths resolve against the configuration file, and `${VAR}` expansion is supported. The default uses the OS temporary directory and is intended for local development. One service process holds the root lock; do not share the directory between independent server instances. Startup inventories all objects before admitting writes, including objects belonging to sessions that have not been opened.

Saved results contain ordinary files under `objects/<id>/`. Tools and the dashboard show server-local absolute paths. Use `read_file` with a line `offset`, `grep`, or `shell` to inspect retained content; `read_file` cuts lines longer than 2000 bytes, so use `grep` or `shell` for the rest of a very long line. Existing filesystem permissions and tool approval rules still apply; the output root is not a new access-control boundary.

The default retention is 24 hours. Expired objects are cleaned every 60 seconds in bounded batches; quota pressure may evict unpinned objects sooner. Payload, metadata and in-flight reservations count against storage limits. Active writers and results awaiting checkpoint cannot be evicted. Deletion failures remain charged and are retried.

Fork copies references without copying files or extending retention. Rewind keeps files and already-committed read progress. Deleting a session stops its writers and removes session metadata, but retained output remains charged until cleanup, so deletion alone does not invalidate paths inherited by a fork. Moving the root or moving the server to another host does not rewrite historical absolute paths.

## Failures and cancellation

A command keeps running when saved output reaches a quota or a write fails. The collector continues draining its pipes, keeps bounded first/last previews, and reports incomplete storage separately from the exit status. A result reference is published only after channel files and the manifest have been durably sealed. Failed or timed-out sealing returns the original execution result with a preview and storage error; it never publishes an unreliable path. Outstanding IO and residual files stay charged until cleanup actually finishes.

Cancellation terminates execution immediately, then allows bounded pipe draining and a shared one-second output finalization deadline. On healthy storage, already-captured intermediate log lines remain accessible through the retained path. Cancelled tool effects are not committed just because their logs were saved.

PTC receives original intermediate tool values within its native-buffer and JS-heap limits, without a cumulative result-volume cap. Intermediate command spills are temporary and are removed after delivery or failure. Synchronous console output has its own reserved, independently drained queue. The final report and explicit logs use the same output boundary as other tools; oversized model responses become a plain-text preview followed by one-line notes giving the truncation reason and the saved file paths. An output delivery failure does not undo a host tool's external effects or authorize an automatic retry.

## Background output and upgrades

`task_output` returns bounded pages. Shell cursors are per consumer; subagent answers support explicit byte continuation. Progress and completion receipts commit with the tool-result checkpoint. Failed deliveries or checkpoints do not skip data. Dashboard pagination and ordinary file reads do not consume tool progress or acknowledge task completion. The dashboard requests another page only when the user asks.

The new database migration adds `task_output_progress`. Old background lifecycle manifests remain recoverable, including task status, pending notices and scope-cleanup facts. On cold open, unfinished tasks become interrupted and their scopes are cleaned before old ring payloads are retired. Old ring/result payloads are not migrated to the new output store; their output becomes unavailable. Back up any old logs that must be kept before upgrading. There is no ongoing second quota or ring-storage implementation.

Memory allocated internally by third-party MCP implementations, provider-generated subagent answers before capture, and file-diff artifacts are outside the managed text-output budget.
