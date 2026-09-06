ALTER TABLE thread_checkpoints RENAME TO process_checkpoints;
ALTER TABLE process_checkpoints RENAME COLUMN thread_id TO pid;
ALTER TABLE process_checkpoints RENAME COLUMN parent_thread_id TO parent_pid;
ALTER TABLE process_checkpoints RENAME CONSTRAINT thread_checkpoints_pkey TO process_checkpoints_pkey;
ALTER TABLE process_checkpoints RENAME CONSTRAINT thread_checkpoints_workspace_id_session_id_fkey TO process_checkpoints_workspace_id_session_id_fkey;
ALTER TABLE messages RENAME COLUMN thread_id TO pid;
ALTER TABLE aborted_executions RENAME COLUMN thread_id TO pid;

-- Rename only runtime-owned metadata; message bodies and tool results are opaque.
DO $$
DECLARE
    row_record record;
    migrated jsonb;
    map_name text;
    queue record;
    queues jsonb;
    envelopes jsonb;
    envelope jsonb;
BEGIN
    FOR row_record IN SELECT workspace_id, session_id, snapshot FROM runtime_snapshots FOR UPDATE LOOP
        migrated := row_record.snapshot;
        IF migrated ? 'active_threads' THEN
            migrated := (migrated - 'active_threads')
                || jsonb_build_object('active_processes', migrated -> 'active_threads');
        END IF;
        FOREACH map_name IN ARRAY ARRAY['drained_envelopes', 'agent_drained_envelopes'] LOOP
            IF NOT (migrated ? map_name) THEN
                CONTINUE;
            END IF;
            queues := '{}'::jsonb;
            FOR queue IN SELECT key, value FROM jsonb_each(migrated -> map_name) LOOP
                envelopes := '[]'::jsonb;
                FOR envelope IN SELECT value FROM jsonb_array_elements(queue.value) WITH ORDINALITY ORDER BY ordinality LOOP
                    IF (envelope -> 'to') ? 'thread_id' THEN
                        envelope := jsonb_set(envelope, '{to}',
                            ((envelope -> 'to') - 'thread_id')
                            || jsonb_build_object('pid', envelope #> '{to,thread_id}'));
                    END IF;
                    IF (envelope #> '{from,Agent}') ? 'thread_id' THEN
                        envelope := jsonb_set(envelope, '{from,Agent}',
                            ((envelope #> '{from,Agent}') - 'thread_id')
                            || jsonb_build_object('pid', envelope #> '{from,Agent,thread_id}'));
                    END IF;
                    envelopes := envelopes || jsonb_build_array(envelope);
                END LOOP;
                queues := queues || jsonb_build_object(queue.key, envelopes);
            END LOOP;
            migrated := jsonb_set(migrated, ARRAY[map_name], queues);
        END LOOP;
        UPDATE runtime_snapshots SET snapshot = migrated
        WHERE workspace_id = row_record.workspace_id AND session_id = row_record.session_id
            AND snapshot IS DISTINCT FROM migrated;
    END LOOP;
END $$;

UPDATE process_checkpoints
SET active_execution = jsonb_set(active_execution, '{completion,Caller}',
    ((active_execution #> '{completion,Caller}') - 'sender_thread_id')
    || jsonb_build_object('sender_pid', active_execution #> '{completion,Caller,sender_thread_id}'))
WHERE (active_execution #> '{completion,Caller}') ? 'sender_thread_id';
