-- Tool state moves onto the message it was already anchored to.
--
-- `thread_state` kept it in a side table keyed by `(message_id, kind)`, but only
-- a successful tool call ever records state, so every entry already had exactly
-- one message to hang off. As a column that anchor is the row itself: a message
-- and its state cannot be written apart, and a fork or a rewind carries the
-- state with no SQL of its own.
--
-- `kind` was part of the old primary key, so the column holds a `{kind: value}`
-- object. Not nullable: recording nothing and recording an empty map are the
-- same thing to every reader. A constant default fills existing rows from the
-- catalog rather than rewriting the table.
alter table messages add column state jsonb not null default '{}'::jsonb;

update messages m
   set state = s.value
  from (
        select workspace_id, session_id, message_id,
               jsonb_object_agg(kind, value) as value
          from thread_state
         group by workspace_id, session_id, message_id
       ) s
 where m.workspace_id = s.workspace_id
   and m.session_id = s.session_id
   and m.message_id = s.message_id;

drop table thread_state;
