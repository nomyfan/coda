-- No `if exists`: diesel only runs this for a migration it recorded as applied,
-- so all four tables are there. A missing one means the schema drifted from the
-- migration log, which should fail loudly rather than be swallowed.
--
-- `sessions` last: the other three reference it, and dropping it first would be
-- refused (or would silently take them along under `cascade`, hiding a mistake
-- in this file).
drop table runtime_snapshots;
drop table messages;
drop table thread_checkpoints;
drop table sessions;
