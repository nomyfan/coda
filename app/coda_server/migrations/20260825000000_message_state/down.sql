create table thread_state (
    workspace_id text  not null,
    session_id   text  not null,
    message_id   uuid  not null,
    kind         text  not null,
    value        jsonb not null,
    primary key (workspace_id, session_id, message_id, kind),
    foreign key (workspace_id, session_id, message_id)
        references messages (workspace_id, session_id, message_id) on delete cascade
);

-- The lateral yields nothing for `{}`, which drops those messages from the join.
insert into thread_state (workspace_id, session_id, message_id, kind, value)
select workspace_id, session_id, message_id, entry.key, entry.value
  from messages, lateral jsonb_each(messages.state) as entry;

alter table messages drop column state;
