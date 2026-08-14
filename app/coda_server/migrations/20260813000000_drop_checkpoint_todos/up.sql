-- The todo list was a bare current value on the checkpoint row: last write wins,
-- with nothing tying it to a point in the conversation. A fork copied it as it
-- stood and a rewind left it alone, so both landed a list describing work the
-- surviving history no longer contains.
--
-- What replaces it is `thread_state` (next migration), where every value is
-- anchored to the message that recorded it and is therefore cut by the same rule
-- as the conversation.
alter table thread_checkpoints drop column todos;
