ALTER TABLE thread_checkpoints RENAME COLUMN reply_target TO active_execution;

CREATE TABLE task_notice_receipts (
    workspace_id text NOT NULL,
    session_id text NOT NULL,
    task_id text NOT NULL,
    message_id uuid NOT NULL,
    PRIMARY KEY (workspace_id, session_id, task_id),
    FOREIGN KEY (workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE CASCADE
);

CREATE TABLE aborted_executions (
    workspace_id text NOT NULL,
    session_id text NOT NULL,
    thread_id text NOT NULL,
    invocation_id text NOT NULL,
    PRIMARY KEY (workspace_id, session_id, thread_id, invocation_id),
    FOREIGN KEY (workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE CASCADE
);
