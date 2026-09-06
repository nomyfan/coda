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
        IF migrated ? 'active_processes' THEN
            migrated := (migrated - 'active_processes')
                || jsonb_build_object('active_threads', migrated -> 'active_processes');
        END IF;
        FOREACH map_name IN ARRAY ARRAY['drained_envelopes', 'agent_drained_envelopes'] LOOP
            IF NOT (migrated ? map_name) THEN
                CONTINUE;
            END IF;
            queues := '{}'::jsonb;
            FOR queue IN SELECT key, value FROM jsonb_each(migrated -> map_name) LOOP
                envelopes := '[]'::jsonb;
                FOR envelope IN SELECT value FROM jsonb_array_elements(queue.value) WITH ORDINALITY ORDER BY ordinality LOOP
                    IF (envelope -> 'to') ? 'pid' THEN
                        envelope := jsonb_set(envelope, '{to}',
                            ((envelope -> 'to') - 'pid')
                            || jsonb_build_object('thread_id', envelope #> '{to,pid}'));
                    END IF;
                    IF (envelope #> '{from,Agent}') ? 'pid' THEN
                        envelope := jsonb_set(envelope, '{from,Agent}',
                            ((envelope #> '{from,Agent}') - 'pid')
                            || jsonb_build_object('thread_id', envelope #> '{from,Agent,pid}'));
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
    ((active_execution #> '{completion,Caller}') - 'sender_pid')
    || jsonb_build_object('sender_thread_id', active_execution #> '{completion,Caller,sender_pid}'))
WHERE (active_execution #> '{completion,Caller}') ? 'sender_pid';

ALTER TABLE aborted_executions RENAME COLUMN pid TO thread_id;
ALTER TABLE messages RENAME COLUMN pid TO thread_id;
ALTER TABLE process_checkpoints RENAME COLUMN parent_pid TO parent_thread_id;
ALTER TABLE process_checkpoints RENAME COLUMN pid TO thread_id;
ALTER TABLE process_checkpoints RENAME CONSTRAINT process_checkpoints_pkey TO thread_checkpoints_pkey;
ALTER TABLE process_checkpoints RENAME CONSTRAINT process_checkpoints_workspace_id_session_id_fkey TO thread_checkpoints_workspace_id_session_id_fkey;
ALTER TABLE process_checkpoints RENAME TO thread_checkpoints;
