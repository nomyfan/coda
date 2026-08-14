-- No `if exists`: diesel only runs this for a migration it recorded as applied.
drop table thread_state;
