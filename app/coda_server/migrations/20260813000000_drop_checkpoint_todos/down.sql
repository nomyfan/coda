-- Restored exactly as it was: `not null` with no default. The default here only
-- exists to give the rows already in the table a value, and goes again once they
-- have one.
alter table thread_checkpoints add column todos jsonb not null default '[]'::jsonb;
alter table thread_checkpoints alter column todos drop default;
