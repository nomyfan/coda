ALTER TABLE thread_checkpoints RENAME TO process_checkpoints;
ALTER TABLE process_checkpoints RENAME COLUMN thread_id TO pid;
ALTER TABLE process_checkpoints RENAME COLUMN parent_thread_id TO parent_pid;
ALTER TABLE process_checkpoints RENAME CONSTRAINT thread_checkpoints_pkey TO process_checkpoints_pkey;
ALTER TABLE process_checkpoints RENAME CONSTRAINT thread_checkpoints_workspace_id_session_id_fkey TO process_checkpoints_workspace_id_session_id_fkey;
ALTER TABLE messages RENAME COLUMN thread_id TO pid;
ALTER TABLE aborted_executions RENAME COLUMN thread_id TO pid;
