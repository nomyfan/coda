ALTER TABLE aborted_executions RENAME COLUMN pid TO thread_id;
ALTER TABLE messages RENAME COLUMN pid TO thread_id;
ALTER TABLE process_checkpoints RENAME COLUMN parent_pid TO parent_thread_id;
ALTER TABLE process_checkpoints RENAME COLUMN pid TO thread_id;
ALTER TABLE process_checkpoints RENAME CONSTRAINT process_checkpoints_pkey TO thread_checkpoints_pkey;
ALTER TABLE process_checkpoints RENAME CONSTRAINT process_checkpoints_workspace_id_session_id_fkey TO thread_checkpoints_workspace_id_session_id_fkey;
ALTER TABLE process_checkpoints RENAME TO thread_checkpoints;
