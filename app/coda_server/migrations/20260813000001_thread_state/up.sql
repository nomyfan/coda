-- Tool state, anchored to the message whose call produced it.
--
-- The anchor is what makes this cut with the conversation: a fork copies
-- message_ids verbatim under the new session, and a rewind deletes the message
-- rows themselves — so `on delete cascade` truncates this table with them and a
-- rewind needs no code of its own. Each row holds a *complete* value for its
-- kind, never a delta, which is what lets a range be collapsed (as a compaction
-- would) by keeping the last row per kind.
--
-- The foreign key targets the `(workspace_id, session_id, message_id)` unique
-- constraint on `messages` rather than its primary key, which carries
-- `thread_id` and `seq`. That is deliberate: the anchor names a message, and the
-- message names its thread, so nothing here has to be remapped when a fork
-- rebuilds thread ids.
create table thread_state (
    workspace_id text  not null,
    session_id   text  not null,
    -- The message this value was recorded against.
    message_id   uuid  not null,
    -- Opaque to the server: whoever writes a kind is the only thing that knows
    -- what it holds.
    kind         text  not null,
    value        jsonb not null,
    primary key (workspace_id, session_id, message_id, kind),
    foreign key (workspace_id, session_id, message_id)
        references messages (workspace_id, session_id, message_id) on delete cascade
);
