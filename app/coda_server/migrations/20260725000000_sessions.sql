-- Session persistence. `sessions` is the aggregate root: everything else hangs
-- off it through the composite foreign key, so deleting a session cascades to
-- its threads, messages and runtime snapshot without leaving orphans.
create table sessions (
    workspace_id  text not null,
    session_id    text not null,
    name          text,
    -- SessionModelBinding { provider_id, model_id, reasoning_effort }. Stored
    -- whole because the three fields are always read together and nothing
    -- filters or groups by them.
    model_binding jsonb       not null,
    created_at    timestamptz not null default now(),
    -- Bumped on every checkpoint/snapshot write; orders the session list.
    updated_at    timestamptz not null default now(),
    primary key (workspace_id, session_id)
);

-- One row per agent thread: everything about a thread except its conversation.
-- Bounded in size, so it stays whole-row upsert + JSONB.
create table thread_checkpoints (
    workspace_id     text not null,
    session_id       text not null,
    thread_id        text not null,
    agent_name       text not null,
    -- The thread that spawned this one, and the string its id was derived from
    -- (`uuid5(parent_thread_id, derivation_key)`). Both null on the root thread,
    -- whose thread_id is the session_id.
    parent_thread_id text,
    derivation_key   text,
    reply_target     jsonb,
    resume_point     jsonb       not null,
    todos            jsonb       not null,
    suspended_at     timestamptz not null,
    -- How many messages of this thread are already in `messages`; the next
    -- append starts here, and it is also the next free `seq`.
    message_count    int         not null,
    -- Derived from resume_point so the session list can answer "does this
    -- session need attention?" without reading every checkpoint.
    pending_approval boolean     not null,
    primary key (workspace_id, session_id, thread_id),
    foreign key (workspace_id, session_id)
        references sessions (workspace_id, session_id) on delete cascade
);

-- One row per message. This is the only table that grows with a conversation,
-- which is why it is split by row instead of rewritten as one blob.
create table messages (
    workspace_id      text not null,
    session_id        text not null,
    thread_id         text not null,
    -- Position in the thread's history, 0-based. Computed by the storage layer
    -- (`message_count + i`), not a database identity: it has to mean "index in
    -- the message vector", which a table-wide sequence cannot express.
    seq               int  not null,
    message_id        uuid not null,
    -- The root user message whose submission produced this message, in this
    -- thread or any thread below it.
    turn_id           uuid not null,
    role              text not null,
    -- For the user message that opens a sub-agent thread: which call in which
    -- parent assistant message triggered it. Null everywhere else.
    origin_message_id uuid,
    origin_call_id    text,
    payload           jsonb not null,
    -- When the row was written. The message's own timestamps live in `payload`.
    created_at        timestamptz not null default now(),
    primary key (workspace_id, session_id, thread_id, seq),
    -- Per session, not global: a fork copies message rows verbatim under a new
    -- session_id, and turn_id / origin_message_id references stay valid.
    unique (workspace_id, session_id, message_id),
    foreign key (workspace_id, session_id)
        references sessions (workspace_id, session_id) on delete cascade
);

-- Supports collecting or truncating one submission across every thread it
-- reached, which is what a rewind does.
create index messages_turn on messages (workspace_id, session_id, turn_id);

create table runtime_snapshots (
    workspace_id text  not null,
    session_id   text  not null,
    snapshot     jsonb not null,
    primary key (workspace_id, session_id),
    foreign key (workspace_id, session_id)
        references sessions (workspace_id, session_id) on delete cascade
);
